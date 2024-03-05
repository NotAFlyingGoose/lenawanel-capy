mod builtin;
mod compiler;
mod convert;
mod extend;
mod layout;
mod mangle;

use compiler::program::compile_program;
use cranelift::prelude::isa::{self};
use cranelift::prelude::{settings, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_object::object::write;
use cranelift_object::{ObjectBuilder, ObjectModule};

use hir::FQComptime;
use hir_ty::ComptimeResult;
use interner::Interner;
use rustc_hash::FxHashMap;
use std::mem;
use std::path::PathBuf;
use std::process::{exit, Command};
use target_lexicon::{OperatingSystem, Triple};

#[derive(Debug, PartialEq, Eq)]
pub enum Verbosity {
    None,
    LocalFunctions,
    AllFunctions,
}

pub(crate) type FinalSignature = cranelift::prelude::Signature;

pub use compiler::comptime::eval_comptime_blocks;

pub fn compile_jit(
    verbosity: Verbosity,
    entry_point: hir::Fqn,
    mod_dir: &std::path::Path,
    interner: &Interner,
    world_bodies: &hir::WorldBodies,
    tys: &hir_ty::ProjectInference,
    comptime_results: &FxHashMap<FQComptime, ComptimeResult>,
) -> fn(usize, usize) -> usize {
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").unwrap();
    flag_builder.set("is_pic", "false").unwrap();
    let isa_builder = cranelift_native::builder().unwrap_or_else(|msg| {
        panic!("host machine is not supported: {}", msg);
    });
    let isa = isa_builder
        .finish(settings::Flags::new(flag_builder))
        .unwrap();
    let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());

    let mut module = JITModule::new(builder);

    let cmain = compile_program(
        verbosity,
        entry_point,
        mod_dir,
        interner,
        world_bodies,
        tys,
        &mut module,
        comptime_results,
    );

    // Finalize the functions which were defined, which resolves any
    // outstanding relocations (patching in addresses, now that they're
    // available).
    // This also prepares the code for JIT execution
    module.finalize_definitions().unwrap();

    let code_ptr = module.get_finalized_function(cmain);

    unsafe { mem::transmute::<_, fn(usize, usize) -> usize>(code_ptr) }
}

#[allow(clippy::too_many_arguments)]
pub fn compile_obj(
    verbosity: Verbosity,
    entry_point: hir::Fqn,
    mod_dir: &std::path::Path,
    interner: &Interner,
    world_bodies: &hir::WorldBodies,
    tys: &hir_ty::ProjectInference,
    comptime_results: &FxHashMap<FQComptime, ComptimeResult>,
    target: Triple,
) -> Result<Vec<u8>, write::Error> {
    let mut flag_builder = settings::builder();
    // flag_builder.set("use_colocated_libcalls", "false").unwrap();
    flag_builder.set("is_pic", "true").unwrap();

    let isa_builder = isa::lookup(target).unwrap_or_else(|msg| {
        println!("invalid target: {}", msg);
        exit(1);
    });
    let isa = isa_builder
        .finish(settings::Flags::new(flag_builder))
        .unwrap();

    let builder = ObjectBuilder::new(
        isa,
        entry_point.file.to_string(mod_dir, interner),
        cranelift_module::default_libcall_names(),
    )
    .unwrap();
    let mut module = ObjectModule::new(builder);

    compile_program(
        verbosity,
        entry_point,
        mod_dir,
        interner,
        world_bodies,
        tys,
        &mut module,
        comptime_results,
    );

    // Finalize the functions which were defined, which resolves any
    // outstanding relocations (patching in addresses, now that they're
    // available).
    // This also generates the proper .o
    let product = module.finish();

    product.emit()
}

pub fn link_to_exec(object_file: &PathBuf, target: Triple, libs: Option<&[String]>) -> PathBuf {
    let exe_path = object_file
        .parent()
        .unwrap()
        .join(object_file.file_stem().unwrap());

    let linker_args: &[&str] = match target.operating_system {
        OperatingSystem::MacOSX { .. } => &["-Xlinker", "-ld_classic"],
        _ => &[],
    };

    let success = if let Some(libs) = libs {
        Command::new("gcc")
            .arg(object_file)
            .arg("-o")
            .arg(&exe_path)
            .args(linker_args)
            .args(libs.iter().map(|lib| "-l".to_string() + lib))
            .status()
            .unwrap()
            .success()
    } else {
        Command::new("gcc")
            .arg(object_file)
            .arg("-o")
            .args(linker_args)
            .arg(&exe_path)
            .status()
            .unwrap()
            .success()
    };

    assert!(success);
    exe_path
}

#[cfg(test)]
mod tests {
    use core::panic;
    use std::{env, fs, path::Path};

    use ast::AstNode;
    use expect_test::{expect, Expect};
    use hir_ty::{InferenceCtx, InferenceResult};
    use path_clean::PathClean;
    use target_lexicon::HOST;
    use uid_gen::UIDGenerator;

    use super::*;

    #[track_caller]
    fn check_files(
        main_file: &str,
        other_files: &[&str],
        entry_point: &str,
        stdout_expect: Expect,
        expected_status: i32,
    ) {
        println!("testing {main_file}");

        let current_dir = env!("CARGO_MANIFEST_DIR");
        env::set_current_dir(current_dir).unwrap();

        let mut modules = FxHashMap::default();

        const CORE_DEPS: &[&str] = &[
            "../../core/mod.capy",
            "../../core/ptr.capy",
            "../../core/libc.capy",
            "../../core/math.capy",
            "../../core/meta.capy",
            "../../core/strings.capy",
            "../../core/fmt.capy",
        ];

        for file in other_files.iter().chain(CORE_DEPS.iter()) {
            let file = file.replace('/', std::path::MAIN_SEPARATOR_STR);
            let file = Path::new(current_dir).join(file).clean();
            let text = fs::read_to_string(&file).unwrap();

            modules.insert(file.to_string_lossy().to_string(), text);
        }

        let main_file = main_file.replace('/', std::path::MAIN_SEPARATOR_STR);
        let main_file = Path::new(current_dir).join(main_file).clean();
        let text = fs::read_to_string(&main_file).unwrap();
        modules.insert(main_file.to_string_lossy().to_string(), text);

        check_impl(
            modules
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
            &main_file.to_string_lossy(),
            entry_point,
            false,
            stdout_expect,
            expected_status,
        )
    }

    #[track_caller]
    fn check_raw(input: &str, entry_point: &str, stdout_expect: Expect, expected_status: i32) {
        let modules = test_utils::split_multi_module_test_data(input);

        check_impl(
            modules,
            "main.capy",
            entry_point,
            true,
            stdout_expect,
            expected_status,
        )
    }

    #[track_caller]
    fn check_impl(
        modules: FxHashMap<&str, &str>,
        main_file: &str,
        entry_point: &str,
        fake_file_system: bool,
        stdout_expect: Expect,
        expected_status: i32,
    ) {
        let mod_dir = if fake_file_system {
            std::path::PathBuf::new()
        } else {
            env::current_dir().unwrap().join("../../").clean()
        };

        let mut interner = Interner::default();
        let mut world_index = hir::WorldIndex::default();

        let mut uid_gen = UIDGenerator::default();
        let mut world_bodies = hir::WorldBodies::default();

        for (file, text) in &modules {
            if *file == main_file {
                continue;
            }

            let tokens = lexer::lex(text);
            let parse = parser::parse_source_file(&tokens, text);
            assert_eq!(parse.errors(), &[]);

            let tree = parse.into_syntax_tree();
            let root = ast::Root::cast(tree.root(), &tree).unwrap();
            let (index, diagnostics) = hir::index(root, &tree, &mut interner);

            assert_eq!(diagnostics, vec![]);

            let module = hir::FileName(interner.intern(file));

            let (bodies, diagnostics) = hir::lower(
                root,
                &tree,
                std::path::Path::new(*file),
                &index,
                &mut uid_gen,
                &mut interner,
                &mod_dir,
                fake_file_system,
            );

            assert_eq!(diagnostics, vec![]);

            world_index.add_file(module, index);
            world_bodies.add_file(module, bodies);
        }

        let text = &modules[main_file];
        let file = hir::FileName(interner.intern(main_file));
        let tokens = lexer::lex(text);
        let parse = parser::parse_source_file(&tokens, text);
        assert_eq!(parse.errors(), &[]);

        let tree = parse.into_syntax_tree();
        let root = ast::Root::cast(tree.root(), &tree).unwrap();
        let (index, diagnostics) = hir::index(root, &tree, &mut interner);

        assert_eq!(diagnostics, vec![]);

        let (bodies, diagnostics) = hir::lower(
            root,
            &tree,
            std::path::Path::new(main_file),
            &index,
            &mut uid_gen,
            &mut interner,
            &mod_dir,
            fake_file_system,
        );
        assert_eq!(diagnostics, vec![]);
        world_index.add_file(file, index);
        world_bodies.add_file(file, bodies);

        let entry_point = hir::Fqn {
            file,
            name: hir::Name(interner.intern(entry_point)),
        };

        let mut comptime_results = FxHashMap::default();

        let InferenceResult { tys, .. } =
            InferenceCtx::new(&world_index, &world_bodies, &interner, |comptime, tys| {
                eval_comptime_blocks(
                    Verbosity::LocalFunctions,
                    vec![comptime],
                    &mut comptime_results,
                    Path::new(""),
                    &interner,
                    &world_bodies,
                    tys,
                    HOST.pointer_width().unwrap().bits(),
                );

                comptime_results[&comptime].clone()
            })
            .finish(Some(entry_point), false);
        assert_eq!(diagnostics, vec![]);

        println!("comptime:");

        // evaluate any comptimes that haven't been ran yet
        eval_comptime_blocks(
            Verbosity::AllFunctions,
            world_bodies.find_comptimes(),
            &mut comptime_results,
            &mod_dir,
            &interner,
            &world_bodies,
            &tys,
            HOST.pointer_width().unwrap().bits(),
        );

        println!("actual program:");

        let bytes = compile_obj(
            Verbosity::LocalFunctions,
            entry_point,
            if fake_file_system {
                Path::new("")
            } else {
                &mod_dir
            },
            &interner,
            &world_bodies,
            &tys,
            &comptime_results,
            HOST,
        )
        .unwrap();

        let output_folder = env::current_dir().unwrap().join("test-temp");

        let _ = fs::create_dir(&output_folder);

        let caller = core::panic::Location::caller();
        let out_name = format!("test{}", caller.line());

        let file = output_folder.join(format!("{}.o", out_name));
        fs::write(&file, bytes.as_slice()).unwrap_or_else(|why| {
            panic!("{}: {why}", file.display());
        });

        let exec = link_to_exec(&file, HOST, None);

        let output = std::process::Command::new(exec.clone())
            .output()
            .unwrap_or_else(|_| panic!("{} did not run successfully", exec.display()));

        assert_eq!(output.status.code().unwrap(), expected_status);

        let stdout = std::str::from_utf8(&output.stdout)
            .unwrap()
            .replace('\r', "");
        let stdout = format!("{}\n", stdout);

        println!("stdout: {:?}", stdout);

        dbg!(&stdout_expect.data());
        println!("expected: {:?}", trim_indent(stdout_expect.data()));
        stdout_expect.assert_eq(&stdout);
    }

    fn trim_indent(mut text: &str) -> String {
        if text.starts_with('\n') {
            text = &text[1..];
        }
        let indent = text
            .lines()
            .filter(|it| !it.trim().is_empty())
            .map(|it| it.len() - it.trim_start().len())
            .min()
            .unwrap_or(0);

        lines_with_ends(text)
            .map(|line| {
                if line.len() <= indent {
                    line.trim_start_matches(' ')
                } else {
                    &line[indent..]
                }
            })
            .collect()
    }

    fn lines_with_ends(text: &str) -> LinesWithEnds {
        LinesWithEnds { text }
    }

    struct LinesWithEnds<'a> {
        text: &'a str,
    }

    impl<'a> Iterator for LinesWithEnds<'a> {
        type Item = &'a str;
        fn next(&mut self) -> Option<&'a str> {
            if self.text.is_empty() {
                return None;
            }
            let idx = self.text.find('\n').map_or(self.text.len(), |it| it + 1);
            let (res, next) = self.text.split_at(idx);
            self.text = next;
            Some(res)
        }
    }

    #[test]
    fn hello_world() {
        check_files(
            "../../examples/hello_world.capy",
            &[],
            "main",
            expect![[r#"
            Hello, World!

            "#]],
            0,
        )
    }

    #[test]
    fn vectors() {
        check_files(
            "../../examples/vectors.capy",
            &[],
            "main",
            expect![[r#"
            my_point: (1, 2, 3)

            "#]],
            0,
        )
    }

    #[test]
    fn fib() {
        check_files(
            "../../examples/fib.capy",
            &["../../examples/io.capy"],
            "main",
            expect![[r#"
            Running Fibonacci(28) x 5 times...
            Ready... Go!
            Fibonacci number #28 = 317811

            "#]],
            0,
        )
    }

    #[test]
    fn drink() {
        check_files(
            "../../examples/drink.capy",
            &[],
            "main",
            expect![[r#"
            you can drink

            "#]],
            0,
        )
    }

    #[test]
    fn arrays() {
        check_files(
            "../../examples/arrays.capy",
            &[],
            "main",
            expect![[r#"
            4
            8
            15
            16
            23
            42
            
            "#]],
            0,
        )
    }

    #[test]
    fn array_of_arrays() {
        check_files(
            "../../examples/arrays_of_arrays.capy",
            &[],
            "main",
            expect![[r#"
                my_array[0][0][0] = 2
                my_array[0][0][1] = 4
                my_array[0][0][2] = 6
                my_array[0][1][0] = 2
                my_array[0][1][1] = 4
                my_array[0][1][2] = 6
                my_array[0][2][0] = 2
                my_array[0][2][1] = 4
                my_array[0][2][2] = 6
                my_array[1][0][0] = 2
                my_array[1][0][1] = 4
                my_array[1][0][2] = 6
                my_array[1][1][0] = 127
                my_array[1][1][1] = 0
                my_array[1][1][2] = 42
                my_array[1][2][0] = 2
                my_array[1][2][1] = 4
                my_array[1][2][2] = 6
                my_array[2][0][0] = 2
                my_array[2][0][1] = 4
                my_array[2][0][2] = 6
                my_array[2][1][0] = 2
                my_array[2][1][1] = 4
                my_array[2][1][2] = 6
                my_array[2][2][0] = 2
                my_array[2][2][1] = 4
                my_array[2][2][2] = 6

                global[0][0][0] = 105
                global[0][0][1] = 115
                global[0][0][2] = 125
                global[0][1][0] = 105
                global[0][1][1] = 115
                global[0][1][2] = 125
                global[0][2][0] = 105
                global[0][2][1] = 115
                global[0][2][2] = 125
                global[1][0][0] = 105
                global[1][0][1] = 115
                global[1][0][2] = 125
                global[1][1][0] = 105
                global[1][1][1] = 115
                global[1][1][2] = 125
                global[1][2][0] = 105
                global[1][2][1] = 115
                global[1][2][2] = 125
                global[2][0][0] = 105
                global[2][0][1] = 115
                global[2][0][2] = 125
                global[2][1][0] = 105
                global[2][1][1] = 115
                global[2][1][2] = 125
                global[2][2][0] = 105
                global[2][2][1] = 115
                global[2][2][2] = 125

            "#]],
            0,
        )
    }

    #[test]
    fn slices() {
        check_files(
            "../../examples/slices.capy",
            &[],
            "main",
            expect![[r#"
            { 4, 8, 15, 16, 23, 42 }
            { 1, 2, 3 }
            { 4, 5, 6, 7, 8 }
            { 4, 8, 15, 16, 23, 42 }
            { 4, 8, 15, 16, 23, 42 }
            
            "#]],
            0,
        )
    }

    #[test]
    fn files() {
        check_files(
            "../../examples/files.capy",
            &[],
            "main",
            expect![[r#"
            writing to hello.txt
            reading from hello.txt
            Hello, World!

            "#]],
            0,
        )
    }

    #[test]
    fn ptr_assign() {
        check_files(
            "../../examples/ptr_assign.capy",
            &[],
            "main",
            expect![[r#"
            x = 5
            x = 25

            x = { 1, 2, 3 }
            x = { 1, 42, 3 }

            "#]],
            0,
        )
    }

    #[test]
    fn pretty() {
        check_files(
            "../../examples/pretty.capy",
            &[],
            "main",
            expect![["
                \u{1b}[32mHello!\u{b}\u{1b}[34mWorld\u{1b}[0m

                Joe\u{8}\u{8}\u{8}P
                ALERT!\u{7}
                \u{c}And
                \tnow..
                \u{1b}[1;90mC\u{1b}[91mO\u{1b}[92mL\u{1b}[93m\u{1b}[94mO\u{1b}[95mR\u{1b}[96mS\u{1b}[97m!\u{1b}[0m

            "]],
            0,
        )
    }

    #[test]
    fn float_to_string() {
        check_files(
            "../../examples/float_to_string.capy",
            &[],
            "main",
            expect![[r#"
            3.141

            ln 10 = 2.302
            ln 50 = 3.912
            ln 100 = 4.605
            ln 500 = 6.214
            log 10 = 1.000
            log 50 = 1.698
            log 100 = 1.999
            log 500 = 2.698

            "#]],
            0,
        )
    }

    #[test]
    fn first_class_functions() {
        check_files(
            "../../examples/first_class_functions.capy",
            &[],
            "main",
            expect![[r#"
            apply add to  1 and 2 ... 3
            apply sub to  5 and 3 ... 2
            apply mul to  3 and 2 ... 6
            apply div to 10 and 2 ... 5
            apply pow to  2 and 3 ... 8

            "#]],
            50,
        )
    }

    #[test]
    fn structs() {
        check_files(
            "../../examples/structs.capy",
            &[],
            "main",
            expect![[r#"
            people:
            Bob is 3 years old
            Terry is 48 years old
            Walter is 52 years old

            some_guy:
            Terry is 1000 years old

            "#]],
            0,
        )
    }

    #[test]
    fn comptime() {
        check_files(
            "../../examples/comptime.capy",
            &[],
            "main",
            expect![[r#"
            Hello at runtime!
            that global was equal to 10
            2^0 = 1
            2^1 = 2
            2^2 = 4
            2^3 = 8
            2^4 = 16
            2^5 = 32

            "#]],
            0,
        )
    }

    // comptime_types.capy cannot be tested as it gets user input

    #[test]
    fn string() {
        check_files(
            "../../examples/string.capy",
            &[],
            "main",
            expect![[r#"
            Hello World!

            "#]],
            0,
        )
    }

    #[test]
    fn auto_deref() {
        check_files(
            "../../examples/auto_deref.capy",
            &[],
            "main",
            expect![[r#"
            struct auto deref:
            my_foo.b   8
            ptr^^^^.b  8
            ptr^^^.b   8
            ptr^^.b    8
            ptr^.b     8
            ptr.b      8
              give:
            ptr^^.b    8
            ptr^.b     8
            ptr.b      8

            array auto deref:
            ptr^[0] 4
            ptr[0]  4
            ptr^[1] 8
            ptr[1]  8
            ptr^[2] 15
            ptr[2]  15
            ptr_ptr^^[3] 16
            ptr_ptr^[3]  16
            ptr_ptr[3]   16
            ptr_ptr^^[4] 23
            ptr_ptr^[4]  23
            ptr_ptr[4]   23
            ptr_ptr^^[5] 42
            ptr_ptr^[5]  42
            ptr_ptr[5]   42
              give:
            ptr_ptr^^[0] 4
            ptr_ptr^[0]  4
            ptr_ptr[0]   4
            ptr_ptr^^[1] 8
            ptr_ptr^[1]  8
            ptr_ptr[1]   8
            ptr_ptr^^[2] 15
            ptr_ptr^[2]  15
            ptr_ptr[2]   15
            ptr_ptr^^[3] 16
            ptr_ptr^[3]  16
            ptr_ptr[3]   16
            ptr_ptr^^[4] 23
            ptr_ptr^[4]  23
            ptr_ptr[4]   23
            ptr_ptr^^[5] 42
            ptr_ptr^[5]  42
            ptr_ptr[5]   42

            "#]],
            0,
        )
    }

    #[test]
    fn reflection() {
        check_files(
            "../../examples/reflection.capy",
            &[],
            "main",
            expect![[r#"
                Reflection!
                
                i32              (0x8000284) : size = 4, align = 4, stride = 4
                i64              (0x8000308) : size = 8, align = 8, stride = 8
                u64              (0x8000108) : size = 8, align = 8, stride = 8
                i8               (0x8000221) : size = 1, align = 1, stride = 1
                u128             (0x8000110) : size = 16, align = 8, stride = 16
                usize            (0x8000108) : size = 8, align = 8, stride = 8
                f32              (0xc000084) : size = 4, align = 4, stride = 4
                void             (0x4000020) : size = 0, align = 1, stride = 0
                any              (0x20000020) : size = 0, align = 1, stride = 0
                str              (0x14000108) : size = 8, align = 8, stride = 8
                char             (0x18000021) : size = 1, align = 1, stride = 1
                type             (0x1c000084) : size = 4, align = 4, stride = 4
                Person           (0x40000000) : size = 12, align = 8, stride = 16
                Foo              (0x40000001) : size = 1, align = 1, stride = 1
                [6] Person       (0x48000000) : size = 96, align = 8, stride = 96
                [ ] Person       (0x4c000000) : size = 16, align = 8, stride = 16
                 ^  Person       (0x50000000) : size = 8, align = 8, stride = 8
                distinct Person  (0x44000000) : size = 12, align = 8, stride = 16
                distinct Person  (0x44000001) : size = 12, align = 8, stride = 16
                ()       -> void (0x54000000) : size = 8, align = 8, stride = 8
                (x: i32) -> f32  (0x54000001) : size = 8, align = 8, stride = 8
                
                i32 == i16 : false
                i32 == u32 : false
                i32 == i32 : true
                Foo == Person : false
                Person == Person : true
                [5] Person == [6] Person : false
                [5] Foo == [5] Person : false
                [6] Person == [6] Person : true
                ^Person == ^Foo : false
                ^Person == ^Person : true
                Person == distinct 'a Person : false
                distinct 'a Person == distinct 'b Person : false
                distinct 'b Person == distinct 'b Person : true
                () -> void == (x : i32) -> f32 : false
                () -> void == () -> void : true
                
                INT
                bit_width = 32
                signed    = true
                
                INT
                bit_width = 8
                signed    = false
                
                INT
                bit_width = 128
                signed    = false
                
                INT
                bit_width = 64
                signed    = true
                
                FLOAT
                bit_width = 32
                
                FLOAT
                bit_width = 64
                
                ARRAY
                len = 5
                ty =
                 INT
                 bit_width = 32
                 signed    = true
                
                ARRAY
                len = 1000
                ty =
                 ARRAY
                 len = 3
                 ty =
                  FLOAT
                  bit_width = 64
                
                SLICE
                ty =
                 INT
                 bit_width = 32
                 signed    = true
                
                POINTER
                ty =
                 INT
                 bit_width = 32
                 signed    = true
                
                POINTER
                ty =
                 POINTER
                 ty =
                  POINTER
                  ty =
                   INT
                   bit_width = 128
                   signed    = true
                
                DISTINCT
                ty =
                 INT
                 bit_width = 32
                 signed    = true
                
                DISTINCT
                ty =
                 ARRAY
                 len = 2
                 ty =
                  DISTINCT
                  ty =
                   INT
                   bit_width = 8
                   signed    = true
                
                STRUCT
                members =
                 name = a
                 offset = 0
                 ty =
                  BOOL
                
                STRUCT
                members =
                 name = text
                 offset = 0
                 ty =
                  STRING
                 name = flag
                 offset = 8
                 ty =
                  BOOL
                 name = array
                 offset = 10
                 ty =
                  ARRAY
                  len = 3
                  ty =
                   INT
                   bit_width = 16
                   signed    = true
                
                STRUCT
                members =
                 name = name
                 offset = 0
                 ty =
                  STRING
                 name = age
                 offset = 8
                 ty =
                  INT
                  bit_width = 32
                  signed    = true
                
                STRUCT
                members =
                 name = ty
                 offset = 0
                 ty =
                  META TYPE
                 name = data
                 offset = 8
                 ty =
                  POINTER
                  ty =
                   ANY
                
                DISTINCT
                ty =
                 STRUCT
                 members =
                  name = a
                  offset = 0
                  ty =
                   BOOL
                
                123
                { 4, 8, 15, 16, 23, 42 }
                { 1, 2, 3 }
                ^52
                42
                42
                256
                hello
                { text = Hello, flag = false, array = { 1, 2, 3 } }
                {type}
                {}
                { 4, 8, 15, 16, 23, 42 }
                
            "#]],
            0,
        )
    }

    #[test]
    fn cast_f32_to_i32() {
        check_raw(
            r#"
                main :: () -> i32 {
                    f : f32 = 2.5;

                    f as i32
                }
            "#,
            "main",
            expect![[r#"

"#]],
            2,
        )
    }

    #[test]
    fn local_tys() {
        check_raw(
            r#"
                main :: () -> i32 {
                    int :: i32;
                    imaginary :: distinct int;
                    imaginary_vec3 :: distinct [3] imaginary;
                    complex :: struct {
                        real_part: int,
                        imaginary_part: imaginary,
                    };
                
                    my_complex := complex {
                        real_part: 5,
                        imaginary_part: 42,
                    };
                
                    do_math :: (c: complex) -> imaginary_vec3 {
                        // this is kind of akward because while we can access locals
                        // in the parameters and return type, we can't access `imaginary`
                        // from inside the body of this lambda
                        // this could be alleviated by adding a `type_of` builtin
                        [3] i32 { 1, c.real_part * c.imaginary_part as i32, 3 }
                    };
                
                    do_math(my_complex)[1] as i32
                }
            "#,
            "main",
            expect![[r#"

"#]],
            5 * 42,
        )
    }

    #[test]
    fn logical_operators() {
        check_raw(
            r#"
                a :: (x: bool) -> bool {
                    if x {
                        puts("a: true");
                    } else {
                        puts("a: false");
                    }
                    x
                }

                b :: (x: bool) -> bool {
                    if x {
                        puts("b: true");
                    } else {
                        puts("b: false");
                    }
                    x
                }

                main :: () {
                    puts("logical AND:\n");

                    print_bool(a(true) && b(true));
                    print_bool(a(true) && b(false));
                    print_bool(a(false) && b(true));
                    print_bool(a(false) && b(false));

                    puts("logical OR:\n");

                    print_bool(a(true) || b(true));
                    print_bool(a(true) || b(false));
                    print_bool(a(false) || b(true));
                    print_bool(a(false) || b(false));
                }

                print_bool :: (b: bool) {
                    if b {
                        puts("true\n");
                    } else {
                        puts("false\n");
                    }
                }

                puts :: (s: str) extern;
            "#,
            "main",
            expect![[r#"
                logical AND:

                a: true
                b: true
                true

                a: true
                b: false
                false

                a: false
                false

                a: false
                false

                logical OR:

                a: true
                true

                a: true
                true

                a: false
                b: true
                true

                a: false
                b: false
                false


            "#]],
            0,
        )
    }

    #[test]
    fn control_flow() {
        check_raw(
            r#"
                fib :: (n: i32) -> i32 {
                    if n <= 1 {
                        return n;
                    }
                
                    fib(n - 1) + fib(n - 2)
                }
                
                main :: () -> i32 {
                    {
                        puts("before return");
                        return {
                            puts("before break");
                            x := 5;
                            break loop {
                                res := fib(x);
                                if res > 1_000 {
                                    printf("fib(%i) = %i\n", x, res);
                                    break x;
                                }
                                x = x + 1;
                            };
                            puts("after break");
                            42
                        };
                        puts("after return");
                        1 + 1
                    }
                
                    puts("hello!");
                
                    0
                }
                
                puts :: (s: str) extern;
                printf :: (s: str, n1: i32, n2: i32) -> i32 extern;
            "#,
            "main",
            expect![[r#"
                before return
                before break
                fib(17) = 1597

            "#]],
            17,
        )
    }

    #[test]
    fn break_casting() {
        check_raw(
            r#"
                main :: () -> i64 {
                    {
                        if true {
                            y : i8 = 5;
                            break y;
                        }
    
                        y : i16 = 42;
                        y
                    }
                }
            "#,
            "main",
            expect![[r#"

"#]],
            5,
        )
    }

    #[test]
    fn bitwise_operators() {
        check_raw(
            r#"
                main :: () {
                    printf("~2147483647 =      %i\n", ~{4294967295 as u32});
                    printf(" 5032 &  25 =     %i\n", 5032 & 32);
                    printf(" 5000 |  20 =   %i\n", 5000 | 32);
                    printf(" 5032 ~  36 =   %i\n", 5032 ~ 36);
                    printf(" 5032 &~ 36 =   %i\n", 5032 &~ 36); 
                    printf(" 5032 <<  2 =  %i\n", 5032 << 2);
                    printf(" 5032 >>  2 =   %i\n", 5032 >> 2);
                    printf("-5032 >>  2 =  %i\n", -5032 >> 2);
                }
                
                printf :: (s: str, n: i64) extern;
            "#,
            "main",
            expect![[r#"
                ~2147483647 =      0
                 5032 &  25 =     32
                 5000 |  20 =   5032
                 5032 ~  36 =   5004
                 5032 &~ 36 =   5000
                 5032 <<  2 =  20128
                 5032 >>  2 =   1258
                -5032 >>  2 =  -1258

            "#]],
            0,
        )
    }

    #[test]
    fn early_return() {
        check_raw(
            r#"
                main :: () -> i16 {
                    x := loop {
                        if true {
                            break 123;
                        }
                    };

                    // sometimes early return, sometimes not
                    if true {
                        if true {
                            return x;
                        }
                    } else {
                
                    }
                
                    // always early return
                    {
                        {
                            if true {
                                return 5;
                            } else {
                                return 42;
                            }
                        }
                    }
                
                    0
                }
            "#,
            "main",
            expect![[r#"

"#]],
            123,
        )
    }

    #[test]
    fn void_ptr() {
        check_raw(
            r#"
                main :: () -> i32 {
                    // void variables are given a 0 sized stack allocation
                    x := {};

                    x := x;

                    y := ^x;
                    z := ^x;
                
                    y_raw := {^y as ^any as ^usize}^;
                    z_raw := {^z as ^any as ^usize}^;

                    {y_raw == z_raw} as i32
                }
            "#,
            "main",
            expect![[r#"

"#]],
            1,
        )
    }

    #[test]
    fn r#continue() {
        check_raw(
            r#"
                main :: () {
                    i := 0;
                    loop {
                        i = i + 1;
                
                        if i == 10 {
                            break;
                        }
                
                        if i % 2 == 0 {
                            continue;
                        }
                
                        printf("%i\n", i);
                    }
                }
                
                printf :: (fmt: str, n: i32) extern;
            "#,
            "main",
            expect![[r#"
                1
                3
                5
                7
                9

            "#]],
            0,
        )
    }

    #[test]
    fn defers() {
        check_raw(
            r#"
                main :: () -> i32 {
                    defer printf(" How ye be?");
                    {
                        defer printf(" Sailor!");
                        defer printf("ly");
                        {
                            defer printf(" World");
                            printf("Hello");
                            return 5;
                        }
                    }
                }
                
                printf :: (text: str) extern;
            "#,
            "main",
            expect![[r#"
                Hello Worldly Sailor! How ye be?
            "#]],
            5,
        )
    }

    #[test]
    fn defers_within_defers() {
        check_raw(
            r#"
                main :: () {
                    defer printf("ly Sailor!");
                    defer {
                        defer printf("World");
                        printf("Hello ");
                    };
                }
                
                printf :: (text: str) extern;
            "#,
            "main",
            expect![[r#"
                Hello Worldly Sailor!
            "#]],
            0,
        )
    }

    #[test]
    fn extern_fn_global() {
        check_raw(
            r#"
                main :: () {
                    printf("Hello World!");
                }
                
                printf : (text: str) -> void : extern;
            "#,
            "main",
            expect![[r#"
                Hello World!
            "#]],
            0,
        )
    }

    #[test]
    fn extern_fn_lambda() {
        check_raw(
            r#"
                main :: () {
                    printf("Hello World!");
                }
                
                printf :: (text: str) extern;
            "#,
            "main",
            expect![[r#"
                Hello World!
            "#]],
            0,
        )
    }

    #[test]
    fn comptime_globals_in_comptime_globals() {
        check_raw(
            r#"
                foo :: comptime {
                    puts("comptime global in comptime global");
                    42
                };

                func :: () -> i32 {
                    foo
                }

                bar :: comptime func();

                main :: () -> i32 {
                    baz :: comptime bar;

                    baz;
                    baz;

                    baz
                }
                
                puts :: (text: str) extern;
            "#,
            "main",
            expect![[r#"

"#]],
            42,
        )
    }

    // the "ptrs_to_ptrs.capy" test is not reproducible
}
