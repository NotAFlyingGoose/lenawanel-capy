use cranelift::codegen;
use cranelift::prelude::{
    AbiParam, EntityRef, FunctionBuilder, FunctionBuilderContext, InstBuilder, Signature, Variable,
};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use hir::UIDGenerator;
use hir_ty::ResolvedTy;
use interner::Interner;
use la_arena::Arena;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;

use crate::convert::*;
use crate::functions::FunctionCompiler;
use crate::mangle::Mangle;

pub(crate) struct CodeGen<'a> {
    verbose: bool,

    resolved_arena: &'a Arena<ResolvedTy>,
    interner: &'a Interner,
    bodies_map: &'a FxHashMap<hir::Name, hir::Bodies>,
    tys: &'a hir_ty::InferenceResult,

    builder_context: FunctionBuilderContext,
    ctx: codegen::Context,
    data_description: DataDescription,
    module: &'a mut dyn Module,

    entry_point: hir::Fqn,
    functions_to_compile: VecDeque<hir::Fqn>,

    // globals
    functions: FxHashMap<hir::Fqn, FuncId>,
    data: FxHashMap<hir::Fqn, DataId>,

    str_id_gen: UIDGenerator,
}

impl<'a> CodeGen<'a> {
    pub(crate) fn new(
        verbose: bool,
        entry_point: hir::Fqn,
        resolved_arena: &'a Arena<ResolvedTy>,
        interner: &'a Interner,
        bodies_map: &'a FxHashMap<hir::Name, hir::Bodies>,
        tys: &'a hir_ty::InferenceResult,
        module: &'a mut dyn Module,
    ) -> CodeGen<'a> {
        Self {
            verbose,
            resolved_arena,
            interner,
            bodies_map,
            tys,
            builder_context: FunctionBuilderContext::new(),
            ctx: module.make_context(),
            data_description: DataDescription::new(),
            module,
            entry_point,
            functions_to_compile: VecDeque::from([entry_point]),
            functions: FxHashMap::default(),
            data: FxHashMap::default(),
            str_id_gen: UIDGenerator::default(),
        }
    }

    /// compiles everything into cranelift IR and returns the FuncId of the cmain function
    pub(crate) fn finish(mut self) -> FuncId {
        self.compile_queued_functions();
        self.generate_main_function()
    }

    fn compile_queued_functions(&mut self) {
        while let Some(name) = self.functions_to_compile.pop_front() {
            self.compile_function(name);
            self.compile_queued_functions();
        }
    }

    fn generate_main_function(&mut self) -> FuncId {
        let entry_point = self.get_func_id(self.entry_point);

        let cmain_sig = Signature {
            params: vec![
                AbiParam::new(self.module.target_config().pointer_type()),
                AbiParam::new(self.module.target_config().pointer_type()),
            ],
            returns: vec![AbiParam::new(
                self.module.target_config().pointer_type(), /*isize*/
            )],
            call_conv: self.module.target_config().default_call_conv,
        };
        let cmain_id = self
            .module
            .declare_function("main", Linkage::Export, &cmain_sig)
            .unwrap();

        self.ctx.func.signature = cmain_sig;

        // Create the builder to build a function.
        let mut builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.builder_context);

        // Create the entry block, to start emitting code in.
        let entry_block = builder.create_block();

        builder.switch_to_block(entry_block);
        // tell the builder that the block will have no furthur predecessors
        builder.seal_block(entry_block);

        let arg_argc =
            builder.append_block_param(entry_block, self.module.target_config().pointer_type());
        let arg_argv =
            builder.append_block_param(entry_block, self.module.target_config().pointer_type());

        let var_argc = Variable::new(0);
        builder.declare_var(var_argc, self.module.target_config().pointer_type());
        builder.def_var(var_argc, arg_argc);

        let var_argv = Variable::new(1);
        builder.declare_var(var_argv, self.module.target_config().pointer_type());
        builder.def_var(var_argv, arg_argv);

        let local_entry_point = self.module.declare_func_in_func(entry_point, builder.func);

        let call = builder.ins().call(local_entry_point, &[]);

        let entry_point_signature = self.tys[self.entry_point].as_function().unwrap();

        let exit_code = match entry_point_signature
            .return_ty
            .to_comp_type(self.module, self.resolved_arena)
            .into_int_type()
        {
            Some(return_int_ty) => {
                let exit_code = builder.inst_results(call)[0];

                match return_int_ty
                    .bit_width
                    .cmp(&self.module.target_config().pointer_bits())
                {
                    std::cmp::Ordering::Less => builder
                        .ins()
                        .uextend(self.module.target_config().pointer_type(), exit_code),
                    std::cmp::Ordering::Equal => exit_code,
                    std::cmp::Ordering::Greater => builder
                        .ins()
                        .ireduce(self.module.target_config().pointer_type(), exit_code),
                }
            }
            _ => builder
                .ins()
                .iconst(self.module.target_config().pointer_type(), 0),
        };

        builder.ins().return_(&[exit_code]);

        builder.seal_all_blocks();
        builder.finalize();

        if self.verbose {
            println!("main:\n{}", self.ctx.func);
        }

        self.module
            .define_function(cmain_id, &mut self.ctx)
            .expect("error defining function");

        self.module.clear_context(&mut self.ctx);

        cmain_id
    }

    fn get_func_id(&mut self, fqn: hir::Fqn) -> FuncId {
        get_func_id(
            self.module,
            &mut self.functions,
            &mut self.functions_to_compile,
            self.tys,
            self.resolved_arena,
            self.interner,
            fqn,
        )
    }

    fn compile_function(&mut self, fqn: hir::Fqn) {
        let signature = self.tys[fqn]
            .as_function()
            .expect("tried to compile non-function as function");

        if signature.is_extern {
            return;
        }

        let (comp_sig, new_idx_to_old_idx) =
            signature.to_cranelift_signature(self.module, self.resolved_arena);
        let func_id = self
            .module
            .declare_function(
                &fqn.to_mangled_name(self.interner),
                Linkage::Export,
                &comp_sig,
            )
            .unwrap();
        self.functions.insert(fqn, func_id);

        self.ctx.func.signature = comp_sig.clone();

        // Create the builder to build a function.
        let builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.builder_context);

        let compiler = FunctionCompiler {
            builder,
            fqn,
            signature: comp_sig,
            resolved_arena: self.resolved_arena,
            interner: self.interner,
            bodies_map: self.bodies_map,
            tys: self.tys,
            module: self.module,
            data_description: &mut self.data_description,
            functions_to_compile: &mut self.functions_to_compile,
            functions: &mut self.functions,
            globals: &mut self.data,
            str_id_gen: &mut self.str_id_gen,
            var_id_gen: UIDGenerator::default(),
            locals: FxHashMap::default(),
            params: FxHashMap::default(),
        };

        compiler.finish(signature.return_ty, new_idx_to_old_idx);

        if self.verbose {
            println!(
                "{}.{}:\n{}",
                self.interner.lookup(fqn.module.0),
                self.interner.lookup(fqn.name.0),
                self.ctx.func
            );
        }

        self.module
            .define_function(func_id, &mut self.ctx)
            .expect("error defining function");

        self.module.clear_context(&mut self.ctx);
    }
}

pub(crate) fn get_func_id(
    module: &mut dyn Module,
    functions: &mut FxHashMap<hir::Fqn, FuncId>,
    functions_to_compile: &mut VecDeque<hir::Fqn>,
    tys: &hir_ty::InferenceResult,
    resolved_arena: &Arena<ResolvedTy>,
    interner: &Interner,
    fqn: hir::Fqn,
) -> FuncId {
    if let Some(func_id) = functions.get(&fqn) {
        return *func_id;
    }

    functions_to_compile.push_back(fqn);

    let signature = tys[fqn]
        .as_function()
        .expect("tried to compile non-function as function");

    let (comp_sig, _) = signature.to_cranelift_signature(module, resolved_arena);

    let func_id = if signature.is_extern {
        module
            .declare_function(interner.lookup(fqn.name.0), Linkage::Import, &comp_sig)
            .expect("There are multiple extern functions with the same name")
    } else {
        module
            .declare_function(&fqn.to_mangled_name(interner), Linkage::Export, &comp_sig)
            .unwrap()
    };

    functions.insert(fqn, func_id);

    func_id
}
