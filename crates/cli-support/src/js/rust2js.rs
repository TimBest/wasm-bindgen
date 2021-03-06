use failure::Error;

use descriptor::{Descriptor, Function};
use super::{indent, Context, Js2Rust};

/// Helper struct for manfuacturing a shim in JS used to translate Rust types to
/// JS, then invoking an imported JS function.
pub struct Rust2Js<'a, 'b: 'a> {
    cx: &'a mut Context<'b>,

    /// Arguments of the JS shim that we're generating, aka the variables passed
    /// from Rust which are only numbers.
    shim_arguments: Vec<String>,

    /// Arguments which are forwarded to the imported JS function
    js_arguments: Vec<String>,

    /// Conversions that happen before we invoke the wasm function, such as
    /// converting a string to a ptr/length pair.
    prelude: String,

    /// "Destructors" or cleanup that must happen after the wasm function
    /// finishes. This is scheduled in a `finally` block.
    finally: String,

    /// Next global index to write to when passing arguments via the single
    /// global stack.
    global_idx: usize,

    /// Index of the next argument for unique name generation purposes.
    arg_idx: usize,

    /// Expression used to generate the return value. The string "JS" in this
    /// expression is replaced with the actual JS invocation eventually.
    ret_expr: String,

    /// Whether or not we're catching JS exceptions
    catch: bool,
}

impl<'a, 'b> Rust2Js<'a, 'b> {
    pub fn new(cx: &'a mut Context<'b>) -> Rust2Js<'a, 'b> {
        Rust2Js {
            cx,
            shim_arguments: Vec::new(),
            js_arguments: Vec::new(),
            prelude: String::new(),
            finally: String::new(),
            global_idx: 0,
            arg_idx: 0,
            ret_expr: String::new(),
            catch: false,
        }
    }

    pub fn catch(&mut self, catch: bool) -> &mut Self {
        if catch {
            self.cx.expose_uint32_memory();
            self.cx.expose_add_heap_object();
        }
        self.catch = catch;
        self
    }

    /// Generates all bindings necessary for the signature in `Function`,
    /// creating necessary argument conversions and return value processing.
    pub fn process(&mut self, function: &Function) -> Result<&mut Self, Error> {
        for arg in function.arguments.iter() {
            self.argument(arg)?;
        }
        self.ret(&function.ret)?;
        Ok(self)
    }

    fn argument(&mut self, arg: &Descriptor) -> Result<(), Error> {
        let i = self.arg_idx;
        self.arg_idx += 1;

        self.shim_arguments.push(format!("arg{}", i));

        if let Some(ty) = arg.vector_kind() {
            let f = self.cx.expose_get_vector_from_wasm(ty);
            self.cx.expose_get_global_argument()?;
            let next_global = self.global_idx();
            self.prelude(&format!("\
                let len{0} = getGlobalArgument({next_global});\n\
                let v{0} = {func}(arg{0}, len{0});\n\
            ", i, func = f, next_global = next_global));

            if !arg.is_by_ref() {
                self.prelude(&format!("\
                    wasm.__wbindgen_free(arg{0}, len{0} * {size});\
                ", i, size = ty.size()));
                self.cx.require_internal_export("__wbindgen_free")?;
            }
            self.js_arguments.push(format!("v{}", i));
            return Ok(())
        }

        if let Some(class) = arg.rust_struct() {
            if arg.is_by_ref() {
                bail!("cannot invoke JS functions with custom ref types yet")
            }
            let assign = format!("let c{0} = {1}.__construct(arg{0});", i, class);
            self.prelude(&assign);
            self.js_arguments.push(format!("c{}", i));
            return Ok(())
        }

        if let Some((f, mutable)) = arg.stack_closure() {
            let (js, _ts) = {
                let mut builder = Js2Rust::new("", self.cx);
                if mutable {
                    builder.prelude("let a = this.a;\n")
                        .prelude("this.a = 0;\n")
                        .rust_argument("a")
                        .finally("this.a = a;\n");
                } else {
                    builder.rust_argument("this.a");
                }
                builder
                    .rust_argument("this.b")
                    .process(f)?
                    .finish("function", "this.f")
            };
            self.cx.expose_get_global_argument()?;
            self.cx.function_table_needed = true;
            let next_global = self.global_idx();
            self.global_idx();
            self.prelude(&format!("\
                let cb{0} = {js};\n\
                cb{0}.f = wasm.__wbg_function_table.get(arg{0});\n\
                cb{0}.a = getGlobalArgument({next_global});\n\
                cb{0}.b = getGlobalArgument({next_global} + 1);\n\
            ", i, js = js, next_global = next_global));
            self.finally(&format!("cb{0}.a = cb{0}.b = 0;", i));
            self.js_arguments.push(format!("cb{0}.bind(cb{0})", i));
            return Ok(())
        }

        if let Some(closure) = arg.ref_closure() {
            let (js, _ts) = {
                let mut builder = Js2Rust::new("", self.cx);
                if closure.mutable {
                    builder.prelude("let a = this.a;\n")
                        .prelude("this.a = 0;\n")
                        .rust_argument("a")
                        .finally("this.a = a;\n");
                } else {
                    builder.rust_argument("this.a");
                }
                builder
                    .rust_argument("this.b")
                    .process(&closure.function)?
                    .finish("function", "this.f")
            };
            self.cx.expose_get_global_argument()?;
            self.cx.expose_uint32_memory();
            self.cx.expose_add_heap_object();
            self.cx.function_table_needed = true;
            let reset_idx  = format!("\
                let cb{0} = {js};\n\
                cb{0}.a = getGlobalArgument({a});\n\
                cb{0}.b = getGlobalArgument({b});\n\
                cb{0}.f = wasm.__wbg_function_table.get(getGlobalArgument({c}));\n\
                let real = cb{0}.bind(cb{0});\n\
                real.original = cb{0};\n\
                idx{0} = getUint32Memory()[arg{0} / 4] = addHeapObject(real);\n\
            ",
                i,
                js = js,
                a = self.global_idx(),
                b = self.global_idx(),
                c = self.global_idx(),
            );
            self.prelude(&format!("\
                let idx{0} = getUint32Memory()[arg{0} / 4];\n\
                if (idx{0} === 0xffffffff) {{\n\
                {1}\
                }}\n\
            ", i, indent(&reset_idx)));
            self.cx.expose_get_object();
            self.js_arguments.push(format!("getObject(idx{})", i));
            return Ok(())
        }

        let invoc_arg = match *arg {
            ref d if d.is_number() => format!("arg{}", i),
            Descriptor::Boolean => format!("arg{} !== 0", i),
            Descriptor::Anyref => {
                self.cx.expose_take_object();
                format!("takeObject(arg{})", i)
            }
            ref d if d.is_ref_anyref() => {
                self.cx.expose_get_object();
                format!("getObject(arg{})", i)
            }
            _ => bail!("unimplemented argument type in imported function: {:?}", arg),
        };
        self.js_arguments.push(invoc_arg);
        Ok(())
    }

    fn ret(&mut self, ret: &Option<Descriptor>) -> Result<(), Error> {
        let ty = match *ret {
            Some(ref t) => t,
            None => {
                self.ret_expr = "JS;".to_string();
                return Ok(())
            }
        };
        if ty.is_by_ref() {
            bail!("cannot return a reference from JS to Rust")
        }
        if let Some(ty) = ty.vector_kind() {
            let f = self.cx.pass_to_wasm_function(ty)?;
            self.cx.expose_uint32_memory();
            self.cx.expose_set_global_argument()?;
            self.ret_expr = format!("\
                const [retptr, retlen] = {}(JS);\n\
                setGlobalArgument(retlen, 0);\n\
                return retptr;\n\
            ", f);
            return Ok(())
        }
        if ty.is_number() {
            self.ret_expr = "return JS;".to_string();
            return Ok(())
        }
        self.ret_expr = match *ty {
            Descriptor::Boolean => "return JS ? 1 : 0;".to_string(),
            Descriptor::Anyref => {
                self.cx.expose_add_heap_object();
                "return addHeapObject(JS);".to_string()
            }
            _ => bail!("unimplemented return from JS to Rust: {:?}", ty),
        };
        Ok(())
    }

    pub fn finish(&self, invoc: &str) -> String {
        let mut ret = String::new();
        ret.push_str("function(");
        ret.push_str(&self.shim_arguments.join(", "));
        if self.catch {
            if self.shim_arguments.len() > 0 {
                ret.push_str(", ")
            }
            ret.push_str("exnptr");
        }
        ret.push_str(") {\n");
        ret.push_str(&indent(&self.prelude));

        let mut invoc = self.ret_expr.replace(
            "JS",
            &format!("{}({})", invoc, self.js_arguments.join(", ")),
        );
        if self.catch {
            let catch = "\
                const view = getUint32Memory();\n\
                view[exnptr / 4] = 1;\n\
                view[exnptr / 4 + 1] = addHeapObject(e);\n\
            ";

            invoc = format!("\
            try {{\n\
            {}\
            }} catch (e) {{\n\
            {}\
            }}\
            ", indent(&invoc), indent(catch));
        };

        if self.finally.len() > 0 {
            invoc = format!("\
            try {{\n\
            {}\
            }} finally {{\n\
            {}\
            }}\
            ", indent(&invoc), indent(&self.finally));
        }
        ret.push_str(&indent(&invoc));

        ret.push_str("}\n");
        return ret
    }

    fn global_idx(&mut self) -> usize {
        let ret = self.global_idx;
        self.global_idx += 1;
        ret
    }

    fn prelude(&mut self, s: &str) -> &mut Self {
        for line in s.lines() {
            self.prelude.push_str(line);
            self.prelude.push_str("\n");
        }
        self
    }

    fn finally(&mut self, s: &str) -> &mut Self {
        for line in s.lines() {
            self.finally.push_str(line);
            self.finally.push_str("\n");
        }
        self
    }
}
