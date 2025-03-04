use super::*;
use crate::{
    ast::{TypedModule, TypedStatement, UntypedExpr, UntypedModule},
    build::{Origin, Target},
    error::Error,
    type_::{build_prelude, pretty::Printer},
    uid::UniqueIdGenerator,
    warning::{TypeWarningEmitter, VectorWarningEmitterIO, WarningEmitter, WarningEmitterIO},
};
use itertools::Itertools;
use smol_str::SmolStr;
use std::sync::Arc;
use vec1::Vec1;

use camino::Utf8PathBuf;

mod assert;
mod assignments;
mod errors;
mod functions;
mod imports;
mod pretty;
mod statement_if;
mod type_alias;
mod use_;
mod warnings;

#[macro_export]
macro_rules! assert_infer {
    ($src:expr, $typ:expr $(,)?) => {
        let t = $crate::type_::tests::infer($src);
        assert_eq!(($src, t), ($src, $typ.to_string()),);
    };
}

#[macro_export]
macro_rules! assert_infer_with_module {
    (($name:expr, $module_src:literal), $src:expr, $module:expr $(,)?) => {
        let constructors =
            $crate::type_::tests::infer_module($src, vec![("thepackage", $name, $module_src)]);
        let expected = $crate::type_::tests::stringify_tuple_strs($module);

        assert_eq!(($src, constructors), ($src, expected));
    };
}

#[macro_export]
macro_rules! assert_module_infer {
    ($src:expr, $module:expr $(,)?) => {{
        let constructors = $crate::type_::tests::infer_module($src, vec![]);
        let expected = $crate::type_::tests::stringify_tuple_strs($module);
        assert_eq!(($src, constructors), ($src, expected));
    }};
}

#[macro_export]
macro_rules! assert_module_error {
    ($src:expr) => {
        let output = $crate::type_::tests::module_error($src, vec![]);
        insta::assert_snapshot!(insta::internals::AutoName, output, $src);
    };
}

#[macro_export]
macro_rules! assert_module_syntax_error {
    ($src:expr) => {
        let output = $crate::type_::tests::syntax_error($src);
        insta::assert_snapshot!(insta::internals::AutoName, output, $src);
    };
}

#[macro_export]
macro_rules! assert_error {
    ($src:expr, $error:expr $(,)?) => {
        let result = $crate::type_::tests::compile_statement_sequence($src)
            .expect_err("should infer an error");
        assert_eq!(($src, sort_options($error)), ($src, sort_options(result)),);
    };

    ($src:expr) => {
        let error = $crate::type_::tests::compile_statement_sequence($src)
            .expect_err("should infer an error");
        let error = $crate::error::Error::Type {
            src: $src.into(),
            path: camino::Utf8PathBuf::from("/src/one/two.gleam"),
            error,
        };
        let output = error.pretty_string();
        insta::assert_snapshot!(insta::internals::AutoName, output, $src);
    };
}

#[macro_export]
macro_rules! assert_with_module_error {
    (($name:expr, $module_src:literal), $src:expr $(,)?) => {
        let output =
            $crate::type_::tests::module_error($src, vec![("thepackage", $name, $module_src)]);
        insta::assert_snapshot!(insta::internals::AutoName, output, $src);
    };

    (
        ($name:expr, $module_src:literal),
        ($name2:expr, $module_src2:literal),
        $src:expr $(,)?
    ) => {
        let output = $crate::type_::tests::module_error(
            $src,
            vec![
                ("thepackage", $name, $module_src),
                ("thepackage", $name2, $module_src2),
            ],
        );
        insta::assert_snapshot!(insta::internals::AutoName, output, $src);
    };
}

fn get_warnings(src: &str, deps: Vec<DependencyModule<'_>>) -> Vec<Warning> {
    let warnings = VectorWarningEmitterIO::default();
    _ = compile_module(src, Some(Arc::new(warnings.clone())), deps).unwrap();
    warnings
        .take()
        .into_iter()
        .map(|warning| match warning {
            crate::Warning::Type { warning, .. } => warning,
            crate::Warning::Parse { .. } => panic!("Unexpected parse warning"),
            crate::Warning::InvalidSource { .. } => panic!("Invalid module file name"),
        })
        .collect_vec()
}

fn get_printed_warnings(src: &str, deps: Vec<DependencyModule<'_>>) -> String {
    let warnings = get_warnings(src, deps);
    let mut nocolor = termcolor::Buffer::no_color();
    for warning in warnings {
        let path = Utf8PathBuf::from("/src/warning/wrn.gleam");
        let warning = warning.into_warning(path, src.into());
        warning.pretty(&mut nocolor);
    }
    String::from_utf8(nocolor.into_inner()).expect("Error printing produced invalid utf8")
}

#[macro_export]
macro_rules! assert_warning {
    ($src:expr) => {
        let output = $crate::type_::tests::get_printed_warnings($src, vec![]);
        insta::assert_snapshot!(insta::internals::AutoName, output, $src);
    };

    ($(($package:expr, $name:expr, $module_src:literal)),+, $src:expr) => {
        let output = $crate::type_::tests::get_printed_warnings(
            $src,
            vec![$(($package, $name, $module_src)),*]
        );
        insta::assert_snapshot!(insta::internals::AutoName, output, $src);
    };

    ($src:expr, $warning:expr $(,)?) => {
        let warnings = $crate::type_::tests::get_warnings($src, vec![]);
        assert!(!warnings.is_empty());
        assert_eq!($warning, warnings[0]);
    };

    ($(($name:expr, $module_src:literal)),+, $src:expr, $warning:expr $(,)?) => {
        let warnings = $crate::type_::tests::get_warnings(
            $src,
            vec![$(("thepackage", $name, $module_src)),*],
        );
        assert!(!warnings.is_empty());
        assert_eq!($warning, warnings[0]);
    };
}

#[macro_export]
macro_rules! assert_no_warnings {
    ($src:expr $(,)?) => {
        let warnings = $crate::type_::tests::get_warnings($src, vec![]);
        assert!(warnings.is_empty());
    };
    ($(($package:expr, $name:expr, $module_src:literal)),+, $src:expr $(,)?) => {
        let warnings = $crate::type_::tests::get_warnings(
            $src,
            vec![$(($package, $name, $module_src)),*],
        );
        assert!(warnings.is_empty());
    };
}

fn compile_statement_sequence(src: &str) -> Result<Vec1<TypedStatement>, crate::type_::Error> {
    let ast = crate::parse::parse_statement_sequence(src).expect("syntax error");
    let mut modules = im::HashMap::new();
    let ids = UniqueIdGenerator::new();
    // DUPE: preludeinsertion
    // TODO: Currently we do this here and also in the tests. It would be better
    // to have one place where we create all this required state for use in each
    // place.
    let _ = modules.insert(PRELUDE_MODULE_NAME.into(), build_prelude(&ids));
    crate::type_::ExprTyper::new(&mut crate::type_::Environment::new(
        ids,
        "themodule",
        Target::Erlang,
        &modules,
        &TypeWarningEmitter::null(),
    ))
    .infer_statements(ast)
}

fn infer(src: &str) -> String {
    let mut printer = crate::type_::pretty::Printer::new();
    let result = compile_statement_sequence(src).expect("should successfully infer");
    printer.pretty_print(result.last().type_().as_ref(), 0)
}

pub fn stringify_tuple_strs(module: Vec<(&str, &str)>) -> Vec<(SmolStr, String)> {
    module
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
}

/// A module loaded as a dependency in the tests.
///
/// In order the tuple elements indicate:
/// 1. The package name.
/// 2. The module name.
/// 3. The module source.
type DependencyModule<'a> = (&'a str, &'a str, &'a str);

pub fn infer_module(src: &str, dep: Vec<DependencyModule<'_>>) -> Vec<(SmolStr, String)> {
    let ast = compile_module(src, None, dep).expect("should successfully infer");
    ast.type_info
        .values
        .iter()
        .filter(|(_, v)| v.public)
        .map(|(k, v)| {
            let mut printer = Printer::new();
            (k.clone(), printer.pretty_print(&v.type_, 0))
        })
        .sorted()
        .collect()
}

pub fn compile_module(
    src: &str,
    warnings: Option<Arc<dyn WarningEmitterIO>>,
    dep: Vec<DependencyModule<'_>>,
) -> Result<TypedModule, crate::type_::Error> {
    let ids = UniqueIdGenerator::new();
    let mut modules = im::HashMap::new();
    let warnings = TypeWarningEmitter::new(
        Utf8PathBuf::new(),
        "".into(),
        WarningEmitter::new(
            warnings.unwrap_or_else(|| Arc::new(VectorWarningEmitterIO::default())),
        ),
    );

    // DUPE: preludeinsertion
    // TODO: Currently we do this here and also in the tests. It would be better
    // to have one place where we create all this required state for use in each
    // place.
    let _ = modules.insert(PRELUDE_MODULE_NAME.into(), build_prelude(&ids));

    for (package, name, module_src) in dep {
        let parsed = crate::parse::parse_module(module_src).expect("syntax error");
        let mut ast = parsed.module;
        ast.name = name.into();
        let module = crate::analyse::infer_module(
            Target::Erlang,
            &ids,
            ast,
            Origin::Src,
            &package.into(),
            &modules,
            &warnings,
        )
        .expect("should successfully infer");
        let _ = modules.insert(name.into(), module.type_info);
    }

    let parsed = crate::parse::parse_module(src).expect("syntax error");
    let ast = parsed.module;
    crate::analyse::infer_module(
        Target::Erlang,
        &ids,
        ast,
        Origin::Src,
        &"thepackage".into(),
        &modules,
        &warnings,
    )
}

pub fn module_error(src: &str, deps: Vec<DependencyModule<'_>>) -> String {
    let error = compile_module(src, None, deps).expect_err("should infer an error");
    let error = Error::Type {
        src: src.into(),
        path: Utf8PathBuf::from("/src/one/two.gleam"),
        error,
    };
    error.pretty_string()
}

pub fn syntax_error(src: &str) -> String {
    let error = crate::parse::parse_module(src).expect_err("should trigger an error when parsing");
    let error = Error::Parse {
        src: src.into(),
        path: Utf8PathBuf::from("/src/one/two.gleam"),
        error,
    };
    error.pretty_string()
}

#[test]
fn field_map_reorder_test() {
    let int = |value: &str| UntypedExpr::Int {
        value: value.into(),
        location: SrcSpan { start: 0, end: 0 },
    };

    struct Case {
        arity: u32,
        fields: HashMap<SmolStr, u32>,
        args: Vec<CallArg<UntypedExpr>>,
        expected_result: Result<(), crate::type_::Error>,
        expected_args: Vec<CallArg<UntypedExpr>>,
    }

    impl Case {
        fn test(self) {
            let mut args = self.args;
            let fm = FieldMap {
                arity: self.arity,
                fields: self.fields,
            };
            let location = SrcSpan { start: 0, end: 0 };
            assert_eq!(self.expected_result, fm.reorder(&mut args, location));
            assert_eq!(self.expected_args, args);
        }
    }

    Case {
        arity: 0,
        fields: HashMap::new(),
        args: vec![],
        expected_result: Ok(()),
        expected_args: vec![],
    }
    .test();

    Case {
        arity: 3,
        fields: HashMap::new(),
        args: vec![
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("1"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("2"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("3"),
            },
        ],
        expected_result: Ok(()),
        expected_args: vec![
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("1"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("2"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("3"),
            },
        ],
    }
    .test();

    Case {
        arity: 3,
        fields: [("last".into(), 2)].into(),
        args: vec![
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("1"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("2"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: Some("last".into()),
                value: int("3"),
            },
        ],
        expected_result: Ok(()),
        expected_args: vec![
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("1"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: None,
                value: int("2"),
            },
            CallArg {
                implicit: false,
                location: Default::default(),
                label: Some("last".into()),
                value: int("3"),
            },
        ],
    }
    .test();
}

#[test]
fn infer_module_type_retention_test() {
    let module: UntypedModule = crate::ast::Module {
        documentation: vec![],
        name: "ok".into(),
        definitions: vec![],
        type_info: (),
    };
    let ids = UniqueIdGenerator::new();
    let mut modules = im::HashMap::new();
    // DUPE: preludeinsertion
    // TODO: Currently we do this here and also in the tests. It would be better
    // to have one place where we create all this required state for use in each
    // place.
    let _ = modules.insert(PRELUDE_MODULE_NAME.into(), build_prelude(&ids));
    let module = crate::analyse::infer_module(
        Target::Erlang,
        &ids,
        module,
        Origin::Src,
        &"thepackage".into(),
        &modules,
        &TypeWarningEmitter::null(),
    )
    .expect("Should infer OK");

    assert_eq!(
        module.type_info,
        ModuleInterface {
            origin: Origin::Src,
            package: "thepackage".into(),
            name: "ok".into(),
            // Core type constructors like String and Int are not included
            types: HashMap::new(),
            types_constructors: HashMap::from([
                ("Bool".into(), vec!["True".into(), "False".into()]),
                ("Result".into(), vec!["Ok".into(), "Error".into()])
            ]),
            values: HashMap::new(),
            accessors: HashMap::new(),
        }
    );
}

#[test]
fn simple_exprs() {
    assert_infer!("True", "Bool");
    assert_infer!("False", "Bool");
    assert_infer!("1", "Int");
    assert_infer!("-2", "Int");
    assert_infer!("1.0", "Float");
    assert_infer!("-8.0", "Float");
    assert_infer!("\"ok\"", "String");
    assert_infer!("\"ok\"", "String");
    assert_infer!("[]", "List(a)");
    assert_infer!("4 % 1", "Int");
    assert_infer!("4 > 1", "Bool");
    assert_infer!("4 >= 1", "Bool");
    assert_infer!("4 <= 1", "Bool");
    assert_infer!("4 < 1", "Bool");

    // Numbers with _'s
    assert_infer!("1000_000", "Int");
    assert_infer!("1_000", "Int");
    assert_infer!("1_000.", "Float");
    assert_infer!("10_000.001", "Float");
    assert_infer!("100_000.", "Float");

    // Nil
    assert_infer!("Nil", "Nil");

    // todo
    assert_infer!("todo", "a");
    assert_infer!("1 == todo", "Bool");
    assert_infer!("todo != 1", "Bool");
    assert_infer!("todo + 1", "Int");
    assert_infer!("todo(\"test\") + 1", "Int");

    // hex, octal, and binary literals
    assert_infer!("0xF", "Int");
    assert_infer!("0o11", "Int");
    assert_infer!("0b1010", "Int");

    // scientific notation
    assert_infer!("6.02e23", "Float");
    assert_infer!("6.02e-23", "Float");
}

#[test]
fn assert() {
    assert_infer!("let assert [] = [] 1", "Int");
    assert_infer!("let assert [a] = [1] a", "Int");
    assert_infer!("let assert [a, 2] = [1] a", "Int");
    assert_infer!("let assert [a, .._] = [1] a", "Int");
    assert_infer!("let assert [a, .._,] = [1] a", "Int");
    assert_infer!("fn(x) { let assert [a] = x a }", "fn(List(a)) -> a");
    assert_infer!("fn(x) { let assert [a] = x a + 1 }", "fn(List(Int)) -> Int");
    assert_infer!("let assert _x = 1 2.0", "Float");
    assert_infer!("let assert _ = 1 2.0", "Float");
    assert_infer!("let assert #(tag, x) = #(1.0, 1) x", "Int");
    assert_infer!("fn(x) { let assert #(a, b) = x a }", "fn(#(a, b)) -> a");
    assert_infer!("let assert 5: Int = 5 5", "Int");
}

#[test]
fn lists() {
    assert_infer!("[]", "List(a)");
    assert_infer!("[1]", "List(Int)");
    assert_infer!("[1, 2, 3]", "List(Int)");
    assert_infer!("[[]]", "List(List(a))");
    assert_infer!("[[1.0, 2.0]]", "List(List(Float))");
    assert_infer!("[fn(x) { x }]", "List(fn(a) -> a)");
    assert_infer!("[fn(x) { x + 1 }]", "List(fn(Int) -> Int)");
    assert_infer!("[fn(x) { x }, fn(x) { x + 1 }]", "List(fn(Int) -> Int)");
    assert_infer!("[fn(x) { x + 1 }, fn(x) { x }]", "List(fn(Int) -> Int)");
    assert_infer!("[[], []]", "List(List(a))");
    assert_infer!("[[], [1]]", "List(List(Int))");

    assert_infer!("[1, ..[2, ..[]]]", "List(Int)");
    assert_infer!("[fn(x) { x }, ..[]]", "List(fn(a) -> a)");
    assert_infer!("let x = [1, ..[]] [2, ..x]", "List(Int)");
}

#[test]
fn trailing_comma_lists() {
    assert_infer!("[1, ..[2, ..[],]]", "List(Int)");
    assert_infer!("[fn(x) { x },..[]]", "List(fn(a) -> a)");

    assert_infer!("let f = fn(x) { x } [f, f]", "List(fn(a) -> a)");
    assert_infer!("[#([], [])]", "List(#(List(a), List(b)))");
}

#[test]
fn tuples() {
    assert_infer!("#(1)", "#(Int)");
    assert_infer!("#(1, 2.0)", "#(Int, Float)");
    assert_infer!("#(1, 2.0, 3)", "#(Int, Float, Int)");
    assert_infer!("#(1, 2.0, #(1, 1))", "#(Int, Float, #(Int, Int))",);
}

#[test]
fn expr_fn() {
    assert_infer!("fn(x) { x }", "fn(a) -> a");
    assert_infer!("fn(x) { x }", "fn(a) -> a");
    assert_infer!("fn(x, y) { x }", "fn(a, b) -> a");
    assert_infer!("fn(x, y) { [] }", "fn(a, b) -> List(c)");
    assert_infer!("let x = 1.0 1", "Int");
    assert_infer!("let id = fn(x) { x } id(1)", "Int");
    assert_infer!("let x = fn() { 1.0 } x()", "Float");
    assert_infer!("fn(x) { x }(1)", "Int");
    assert_infer!("fn() { 1 }", "fn() -> Int");
    assert_infer!("fn() { 1.1 }", "fn() -> Float");
    assert_infer!("fn(x) { 1.1 }", "fn(a) -> Float");
    assert_infer!("fn(x) { x }", "fn(a) -> a");
    assert_infer!("let x = fn(x) { 1.1 } x", "fn(a) -> Float");
    assert_infer!("fn(x, y, z) { 1 }", "fn(a, b, c) -> Int");
    assert_infer!("fn(x) { let y = x y }", "fn(a) -> a");
    assert_infer!("let id = fn(x) { x } id(1)", "Int");
    assert_infer!(
        "let constant = fn(x) { fn(y) { x } } let one = constant(1) one(2.0)",
        "Int",
    );

    assert_infer!("fn(f) { f(1) }", "fn(fn(Int) -> a) -> a");
    assert_infer!("fn(f, x) { f(x) }", "fn(fn(a) -> b, a) -> b");
    assert_infer!("fn(f) { fn(x) { f(x) } }", "fn(fn(a) -> b) -> fn(a) -> b");
    assert_infer!(
        "fn(f) { fn(x) { fn(y) { f(x, y) } } }",
        "fn(fn(a, b) -> c) -> fn(a) -> fn(b) -> c",
    );
    assert_infer!(
        "fn(f) { fn(x, y) { f(x)(y) } }",
        "fn(fn(a) -> fn(b) -> c) -> fn(a, b) -> c",
    );
    assert_infer!(
        "fn(f) { fn(x) { let ff = f ff(x) } }",
        "fn(fn(a) -> b) -> fn(a) -> b",
    );
    assert_infer!(
        "fn(f) { fn(x, y) { let ff = f(x) ff(y) } }",
        "fn(fn(a) -> fn(b) -> c) -> fn(a, b) -> c",
    );
    assert_infer!("fn(x) { fn(y) { x } }", "fn(a) -> fn(b) -> a");
    assert_infer!("fn(f) { f() }", "fn(fn() -> a) -> a");
    assert_infer!("fn(f, x) { f(f(x)) }", "fn(fn(a) -> a, a) -> a");
    assert_infer!(
        "let id = fn(a) { a } fn(x) { x(id) }",
        "fn(fn(fn(a) -> a) -> b) -> b",
    );

    assert_infer!("let add = fn(x, y) { x + y } add(_, 2)", "fn(Int) -> Int");
    assert_infer!("fn(x) { #(1, x) }", "fn(a) -> #(Int, a)");
    assert_infer!("fn(x, y) { #(x, y) }", "fn(a, b) -> #(a, b)");
    assert_infer!("fn(x) { #(x, x) }", "fn(a) -> #(a, a)");
    assert_infer!("fn(x) -> Int { x }", "fn(Int) -> Int");
    assert_infer!("fn(x) -> a { x }", "fn(a) -> a");
    assert_infer!("fn() -> Int { 2 }", "fn() -> Int");
}

#[test]
fn case() {
    assert_infer!("case 1 { a -> 1 }", "Int");
    assert_infer!("case 1 { a -> 1.0 b -> 2.0 c -> 3.0 }", "Float");
    assert_infer!("case 1 { a -> a }", "Int");
    assert_infer!("case 1 { 1 -> 10 2 -> 20 x -> x * 10 }", "Int");
    assert_infer!("case 2.0 { 2.0 -> 1 x -> 0 }", "Int");
    assert_infer!(r#"case "ok" { "ko" -> 1 x -> 0 }"#, "Int");
}

#[test]
fn multiple_subject_case() {
    assert_infer!("case 1, 2.0 { a, b -> a }", "Int");
    assert_infer!("case 1, 2.0 { a, b -> b }", "Float");
    assert_infer!("case 1, 2.0, 3 { a, b, c -> a + c }", "Int");
}

#[test]
fn tuple_index() {
    assert_infer!("#(1, 2.0).0", "Int");
    assert_infer!("#(1, 2.0).1", "Float");
}

#[test]
fn pipe() {
    assert_infer!("1 |> fn(x) { x }", "Int");
    assert_infer!("1.0 |> fn(x) { x }", "Float");
    assert_infer!("let id = fn(x) { x } 1 |> id", "Int");
    assert_infer!("let id = fn(x) { x } 1.0 |> id", "Float");
    assert_infer!("let add = fn(x, y) { x + y } 1 |> add(_, 2)", "Int");
    assert_infer!("let add = fn(x, y) { x + y } 1 |> add(2, _)", "Int");
    assert_infer!("let add = fn(x, y) { x + y } 1 |> add(2)", "Int");
    assert_infer!("let id = fn(x) { x } 1 |> id()", "Int");
    assert_infer!("let add = fn(x) { fn(y) { y + x } } 1 |> add(1)", "Int");
    assert_infer!(
        "let add = fn(x, _, _) { fn(y) { y + x } } 1 |> add(1, 2, 3)",
        "Int"
    );
}

#[test]
fn bit_strings() {
    assert_infer!("let <<x>> = <<1>> x", "Int");
    assert_infer!("let <<x>> = <<1>> x", "Int");
    assert_infer!("let <<x:float>> = <<1>> x", "Float");
    assert_infer!("let <<x:binary>> = <<1>> x", "BitString");
    assert_infer!("let <<x:bytes>> = <<1>> x", "BitString");
    assert_infer!("let <<x:bit_string>> = <<1>> x", "BitString");
    assert_infer!("let <<x:bits>> = <<1>> x", "BitString");

    assert_infer!("let <<x:utf8_codepoint>> = <<128013:32>> x", "UtfCodepoint");
    assert_infer!(
        "let <<x:utf16_codepoint>> = <<128013:32>> x",
        "UtfCodepoint"
    );
    assert_infer!(
        "let <<x:utf32_codepoint>> = <<128013:32>> x",
        "UtfCodepoint"
    );

    assert_infer!(
        "let a = <<1>> let <<x:binary>> = <<1, a:2-bit_string>> x",
        "BitString"
    );
    assert_infer!(
        "let x = <<<<1>>:bit_string, <<2>>:bit_string>> x",
        "BitString"
    );
}

#[test]
fn infer_module_test() {
    assert_module_infer!(
        "pub fn repeat(i, x) {
           case i {
             0 -> []
             i -> [x .. repeat(i - 1, x)]
           }
         }",
        vec![("repeat", "fn(Int, a) -> List(a)")],
    );

    assert_module_infer!(
        "pub fn length(list) {
           case list {
           [] -> 0
           [x .. xs] -> length(xs) + 1
           }
        }",
        vec![("length", "fn(List(a)) -> Int")],
    );

    assert_module_infer!(
        "fn private() { 1 }
         pub fn public() { 1 }",
        vec![("public", "fn() -> Int")],
    );

    assert_module_infer!(
        "pub type Is { Yes No }
         pub fn yes() { Yes }
         pub fn no() { No }",
        vec![
            ("No", "Is"),
            ("Yes", "Is"),
            ("no", "fn() -> Is"),
            ("yes", "fn() -> Is"),
        ],
    );

    assert_module_infer!(
        "pub type Num { I(Int) }
         pub fn one() { I(1) }",
        vec![("I", "fn(Int) -> Num"), ("one", "fn() -> Num")],
    );

    assert_module_infer!(
        "pub type Box(a) { Box(a) }
        pub fn int() { Box(1) }
        pub fn float() { Box(1.0) }",
        vec![
            ("Box", "fn(a) -> Box(a)"),
            ("float", "fn() -> Box(Float)"),
            ("int", "fn() -> Box(Int)"),
        ],
    );

    assert_module_infer!(
        "pub type Singleton { Singleton }
        pub fn go(x) { let Singleton = x 1 }",
        vec![("Singleton", "Singleton"), ("go", "fn(Singleton) -> Int")],
    );

    assert_module_infer!(
        "pub type Box(a) { Box(a) }
        pub fn unbox(x) { let Box(a) = x a }",
        vec![("Box", "fn(a) -> Box(a)"), ("unbox", "fn(Box(a)) -> a")],
    );

    assert_module_infer!(
        "pub type I { I(Int) }
        pub fn open(x) { case x { I(i) -> i  } }",
        vec![("I", "fn(Int) -> I"), ("open", "fn(I) -> Int")],
    );

    assert_module_infer!(
        "pub fn status() { 1 } pub fn list_of(x) { [x] }",
        vec![("list_of", "fn(a) -> List(a)"), ("status", "fn() -> Int")],
    );

    assert_module_infer!(
        "pub external fn go(String) -> String = \"\" \"\"",
        vec![("go", "fn(String) -> String")],
    );

    assert_module_infer!(
        "pub external fn go(Int) -> Float = \"\" \"\"",
        vec![("go", "fn(Int) -> Float")],
    );

    assert_module_infer!(
        "pub external fn go(Int) -> Int = \"\" \"\"",
        vec![("go", "fn(Int) -> Int")],
    );

    assert_module_infer!(
        "pub external fn ok() -> fn(Int) -> Int = \"\" \"\"",
        vec![("ok", "fn() -> fn(Int) -> Int")],
    );

    assert_module_infer!(
        "pub external fn go(Int) -> b = \"\" \"\"",
        vec![("go", "fn(Int) -> a")],
    );

    assert_module_infer!(
        "pub external fn go(Bool) -> b = \"\" \"\"",
        vec![("go", "fn(Bool) -> a")],
    );

    assert_module_infer!(
        "pub external fn go(List(a)) -> a = \"\" \"\"",
        vec![("go", "fn(List(a)) -> a")],
    );

    assert_module_infer!(
        "external fn go(Int) -> b = \"\" \"\"
        pub fn x() { go(1) }",
        vec![("x", "fn() -> a")],
    );

    assert_module_infer!(
        "external fn id(a) -> a = \"\" \"\"
        pub fn i(x) { id(x) }
        pub fn a() { id(1) }
        pub fn b() { id(1.0) }",
        vec![
            ("a", "fn() -> Int"),
            ("b", "fn() -> Float"),
            ("i", "fn(a) -> a"),
        ],
    );

    assert_module_infer!(
        "pub external fn len(List(a)) -> Int = \"\" \"\"",
        vec![("len", "fn(List(a)) -> Int")],
    );

    assert_module_infer!(
        "pub external type Connection\n
         pub external fn is_open(Connection) -> Bool = \"\" \"\"",
        vec![("is_open", "fn(Connection) -> Bool")],
    );

    assert_module_infer!(
        "pub external type Pair(thing, thing)\n
         pub external fn pair(a) -> Pair(a, a) = \"\" \"\"",
        vec![("pair", "fn(a) -> Pair(a, a)")],
    );

    assert_module_infer!(
        "pub fn one() { 1 }
         pub fn zero() { one() - 1 }
         pub fn two() { one() + zero() }",
        vec![
            ("one", "fn() -> Int"),
            ("two", "fn() -> Int"),
            ("zero", "fn() -> Int"),
        ],
    );

    assert_module_infer!(
        "pub fn one() { 1 }
         pub fn zero() { one() - 1 }
         pub fn two() { one() + zero() }",
        vec![
            ("one", "fn() -> Int"),
            ("two", "fn() -> Int"),
            ("zero", "fn() -> Int"),
        ],
    );

    // Structs
    assert_module_infer!(
        "pub type Box { Box(boxed: Int) }",
        vec![("Box", "fn(Int) -> Box")]
    );
    assert_module_infer!(
        "pub type Tup(a, b) { Tup(first: a, second: b) }",
        vec![("Tup", "fn(a, b) -> Tup(a, b)")]
    );
    assert_module_infer!(
        "pub type Tup(a, b, c) { Tup(first: a, second: b, third: c) }
         pub fn third(t) { let Tup(_ , _, third: a) = t a }",
        vec![
            ("Tup", "fn(a, b, c) -> Tup(a, b, c)"),
            ("third", "fn(Tup(a, b, c)) -> c"),
        ],
    );

    // Anon structs
    assert_module_infer!(
        "pub fn ok(x) { #(1, x) }",
        vec![("ok", "fn(a) -> #(Int, a)")],
    );

    assert_module_infer!(
        "pub external fn ok(Int) -> #(Int, Int) = \"\" \"\"",
        vec![("ok", "fn(Int) -> #(Int, Int)")],
    );

    assert_module_infer!(
        "pub external fn go(#(a, c)) -> c = \"\" \"\"",
        vec![("go", "fn(#(a, b)) -> b")],
    );

    assert_module_infer!(
        "pub fn always(ignore _a, return b) { b }",
        vec![("always", "fn(a, b) -> b")],
    );

    // Using types before they are defined

    assert_module_infer!(
        "pub type I { I(Num) } pub type Num { Num }",
        vec![("I", "fn(Num) -> I"), ("Num", "Num")]
    );

    assert_module_infer!(
        "pub type I { I(Num) } pub external type Num",
        vec![("I", "fn(Num) -> I")]
    );
}

#[test]
fn type_alias() {
    assert_module_infer!(
        "type Html = String
         pub fn go() { 1 }",
        vec![("go", "fn() -> Int")],
    );
    assert_module_infer!(
        "type IntString = Result(Int, String)
         pub fn ok_one() -> IntString { Ok(1) }",
        vec![("ok_one", "fn() -> Result(Int, String)")]
    );
}

#[test]
fn build_in_type_alias_shadow() {
    // We can create an alias with the same name as a built in type
    assert_module_infer!(
        "type Int = Float
         pub fn ok_one() -> Int { 1.0 }",
        vec![("ok_one", "fn() -> Float")]
    );
}

#[test]
fn accessor() {
    // We can access fields on custom types with only one variant
    assert_module_infer!(
        "
pub type Person { Person(name: String, age: Int) }
pub fn get_age(person: Person) { person.age }
pub fn get_name(person: Person) { person.name }",
        vec![
            ("Person", "fn(String, Int) -> Person"),
            ("get_age", "fn(Person) -> Int"),
            ("get_name", "fn(Person) -> String"),
        ]
    );

    // We can access fields on custom types with only one variant
    assert_module_infer!(
        "
pub type One { One(name: String) }
pub type Two { Two(one: One) }
pub fn get(x: Two) { x.one.name }",
        vec![
            ("One", "fn(String) -> One"),
            ("Two", "fn(One) -> Two"),
            ("get", "fn(Two) -> String"),
        ]
    );
}

#[test]
fn generic_accessor() {
    // Field access correctly handles type parameters
    assert_module_infer!(
        "
pub type Box(a) { Box(inner: a) }
pub fn get_box(x: Box(Box(a))) { x.inner }
pub fn get_generic(x: Box(a)) { x.inner }
pub fn get_get_box(x: Box(Box(a))) { x.inner.inner }
pub fn get_int(x: Box(Int)) { x.inner }
pub fn get_string(x: Box(String)) { x.inner }
",
        vec![
            ("Box", "fn(a) -> Box(a)"),
            ("get_box", "fn(Box(Box(a))) -> Box(a)"),
            ("get_generic", "fn(Box(a)) -> a"),
            ("get_get_box", "fn(Box(Box(a))) -> a"),
            ("get_int", "fn(Box(Int)) -> Int"),
            ("get_string", "fn(Box(String)) -> String"),
        ]
    );
}

#[test]
fn generic_accessor_later_defined() {
    // Field access works before type is defined
    assert_module_infer!(
        "
pub fn name(cat: Cat) {
  cat.name
}

pub opaque type Cat {
  Cat(name: String)
}",
        vec![("name", "fn(Cat) -> String"),]
    );
}

#[test]
fn custom_type_annotation() {
    // We can annotate let with custom types
    assert_module_infer!(
        "
        pub type Person {
            Person(name: String, age: Int)
        }

        pub fn create_person(name: String) {
            let x: Person = Person(name: name, age: 1)
            x
        }",
        vec![
            ("Person", "fn(String, Int) -> Person"),
            ("create_person", "fn(String) -> Person"),
        ]
    );

    assert_module_infer!(
        "
        pub type Box(inner) {
            Box(inner)
        }

        pub fn create_int_box(value: Int) {
            let x: Box(Int) = Box(value)
            x
        }

        pub fn create_float_box(value: Float) {
            let x: Box(Float) = Box(value)
            x
        }",
        vec![
            ("Box", "fn(a) -> Box(a)"),
            ("create_float_box", "fn(Float) -> Box(Float)"),
            ("create_int_box", "fn(Int) -> Box(Int)"),
        ]
    );
}

#[test]
fn opaque_accessors() {
    // Opaque type constructors are available in the module where they are defined
    // but are not exported
    assert_module_infer!(
        "
pub opaque type One { One(name: String) }
pub fn get(x: One) { x.name }",
        vec![("get", "fn(One) -> String"),]
    );
}

#[test]
fn fn_annotation_reused() {
    // Type variables are shared between function annotations and function
    // annotations within their body
    assert_module_infer!(
        "
        pub type Box(a) {
            Box(value: a)
        }
        pub fn go(box1: Box(a)) {
            fn(box2: Box(a)) { box1.value == box2.value }
        }",
        vec![
            ("Box", "fn(a) -> Box(a)"),
            ("go", "fn(Box(a)) -> fn(Box(a)) -> Bool")
        ]
    );

    // Type variables are shared between function annotations and let
    // annotations within their body
    assert_module_infer!(
        "
        pub type Box(a) {
            Box(value: a)
        }
        pub fn go(box1: Box(a)) {
            let x: Box(a) = box1
            fn(box2: Box(a)) { x.value == box2.value }
        }",
        vec![
            ("Box", "fn(a) -> Box(a)"),
            ("go", "fn(Box(a)) -> fn(Box(a)) -> Bool")
        ]
    );
}

#[test]
fn accessor_multiple_variants() {
    // We can access fields on custom types with multiple variants
    assert_module_infer!(
        "
pub type Person {
    Teacher(name: String, title: String)
    Student(name: String, age: Int)
}
pub fn get_name(person: Person) { person.name }",
        vec![
            ("Student", "fn(String, Int) -> Person"),
            ("Teacher", "fn(String, String) -> Person"),
            ("get_name", "fn(Person) -> String"),
        ]
    );
}

#[test]
fn record_accessor_multiple_variants_parameterised_types() {
    // We can access fields on custom types with multiple variants
    // In positions other than the 1st field
    assert_module_infer!(
        "
pub type Person {
    Teacher(name: String, age: List(Int), title: String)
    Student(name: String, age: List(Int))
}
pub fn get_name(person: Person) { person.name }
pub fn get_age(person: Person) { person.age }",
        vec![
            ("Student", "fn(String, List(Int)) -> Person"),
            ("Teacher", "fn(String, List(Int), String) -> Person"),
            ("get_age", "fn(Person) -> List(Int)"),
            ("get_name", "fn(Person) -> String"),
        ]
    );
}

#[test]
fn accessor_multiple_variants_positions_other_than_first() {
    // We can access fields on custom types with multiple variants
    // In positions other than the 1st field
    assert_module_infer!(
        "
pub type Person {
    Teacher(name: String, age: Int, title: String)
    Student(name: String, age: Int)
}
pub fn get_name(person: Person) { person.name }
pub fn get_age(person: Person) { person.age }",
        vec![
            ("Student", "fn(String, Int) -> Person"),
            ("Teacher", "fn(String, Int, String) -> Person"),
            ("get_age", "fn(Person) -> Int"),
            ("get_name", "fn(Person) -> String"),
        ]
    );
}

#[test]
fn box_record() {
    assert_module_infer!(
        "
pub type Box {
  Box(a: Nil, b: Int, c: Int, d: Int)
}

pub fn main() {
  Box(b: 1, c: 1, d: 1, a: Nil)
}",
        vec![
            ("Box", "fn(Nil, Int, Int, Int) -> Box"),
            ("main", "fn() -> Box"),
        ],
    );
}

#[test]
fn record_update_no_fields() {
    // No arguments given to a record update
    assert_module_infer!(
        "
        pub type Person {
            Person(name: String, age: Int)
        }
        pub fn identity(person: Person) {
            Person(..person)
        }",
        vec![
            ("Person", "fn(String, Int) -> Person"),
            ("identity", "fn(Person) -> Person")
        ]
    );
}

#[test]
fn record_update() {
    // Some arguments given to a record update
    assert_module_infer!(
        "
        pub type Person {
            Person(name: String, age: Int)
        }
        pub fn update_name(person: Person, name: String) {
            Person(..person, name: name)
        }",
        vec![
            ("Person", "fn(String, Int) -> Person"),
            ("update_name", "fn(Person, String) -> Person")
        ]
    );
}

#[test]
fn record_update_all_fields() {
    // All arguments given in order to a record update
    assert_module_infer!(
        "
        pub type Person {
            Person(name: String, age: Int)
        }
        pub fn update_person(person: Person, name: String, age: Int) {
            Person(..person, name: name, age: age, )
        }",
        vec![
            ("Person", "fn(String, Int) -> Person"),
            ("update_person", "fn(Person, String, Int) -> Person")
        ]
    );
}

#[test]
fn record_update_out_of_order() {
    // All arguments given out of order to a record update
    assert_module_infer!(
        "
        pub type Person {
            Person(name: String, age: Int)
        }
        pub fn update_person(person: Person, name: String, age: Int) {
            Person(..person, age: age, name: name)
        }",
        vec![
            ("Person", "fn(String, Int) -> Person"),
            ("update_person", "fn(Person, String, Int) -> Person")
        ]
    );
}

#[test]
fn record_update_generic() {
    // A record update with polymorphic types
    assert_module_infer!(
        "
        pub type Box(a, b) {
            Box(left: a, right: b)
        }

        pub fn combine_boxes(a: Box(Int, Bool), b: Box(Bool, Int)) {
            Box(..a, left: a.left + b.right, right: b.left)
        }",
        vec![
            ("Box", "fn(a, b) -> Box(a, b)"),
            (
                "combine_boxes",
                "fn(Box(Int, Bool), Box(Bool, Int)) -> Box(Int, Bool)"
            )
        ]
    );
}

#[test]
fn record_update_generic_unannotated() {
    // A record update with unannotated polymorphic types
    assert_module_infer!(
        "
        pub type Box(a, b) {
            Box(left: a, right: b)
        }

        pub fn combine_boxes(a: Box(t1, t2), b: Box(t2, t1)) {
            Box(..a, left: b.right, right: b.left)
        }",
        vec![
            ("Box", "fn(a, b) -> Box(a, b)"),
            ("combine_boxes", "fn(Box(a, b), Box(b, a)) -> Box(a, b)")
        ]
    );
}

#[test]
fn module_constants() {
    assert_module_infer!(
        "
    pub const test_int1 = 123
    pub const test_int2: Int = 321
    pub const test_int3 = 0xE
    pub const test_int4 = 0o10
    pub const test_int5 = 0o10011
    pub const test_float: Float = 4.2
    pub const test_string = \"hey!\"
    pub const test_list = [1,2,3]
    pub const test_tuple = #(\"yes!\", 42)
    pub const test_var1 = test_int1
    pub const test_var2: Int = test_int1",
        vec![
            ("test_float", "Float"),
            ("test_int1", "Int"),
            ("test_int2", "Int"),
            ("test_int3", "Int"),
            ("test_int4", "Int"),
            ("test_int5", "Int"),
            ("test_list", "List(Int)"),
            ("test_string", "String"),
            ("test_tuple", "#(String, Int)"),
            ("test_var1", "Int"),
            ("test_var2", "Int")
        ],
    );
}

#[test]
fn custom_type_module_constants() {
    assert_module_infer!(
        "pub type Test { A }
        pub const test = A",
        vec![("A", "Test"), ("test", "Test")],
    );
}

#[test]
fn module_constant_functions() {
    assert_module_infer!(
        "pub fn int_identity(i: Int) -> Int { i }
        pub const int_identity_alias1 = int_identity
        pub const int_identity_alias2 = int_identity_alias1
        pub const int_identity_alias3: fn(Int) -> Int = int_identity_alias2",
        vec![
            ("int_identity", "fn(Int) -> Int"),
            ("int_identity_alias1", "fn(Int) -> Int"),
            ("int_identity_alias2", "fn(Int) -> Int"),
            ("int_identity_alias3", "fn(Int) -> Int"),
        ]
    );
}

#[test]
fn functions_used_before_definition() {
    assert_module_infer!(
        "pub fn a() { b() }
         pub fn b() { 1 }",
        vec![("a", "fn() -> Int"), ("b", "fn() -> Int")],
    );

    assert_module_infer!(
        "pub fn a() { b() + c() }
         fn b() { 1 }
         fn c() { 1 }",
        vec![("a", "fn() -> Int")],
    );

    assert_module_infer!(
        "fn b() { 1 }
         pub fn a() { b() + c() }
         fn c() { 1 }",
        vec![("a", "fn() -> Int")],
    );

    assert_module_infer!(
        "pub fn a() { Thing }
         pub type Thing { Thing }",
        vec![("Thing", "Thing"), ("a", "fn() -> Thing"),],
    );

    assert_module_infer!(
        "pub fn a() { b()  }
         external fn b() -> Int = \"\" \"\"",
        vec![("a", "fn() -> Int")],
    );
}

#[test]
fn types_used_before_definition() {
    assert_module_infer!(
        "pub type Y { Y(X) }
         pub external type X",
        vec![("Y", "fn(X) -> Y")],
    );

    assert_module_infer!(
        "pub type Y { Y(x: X) }
         pub external type X",
        vec![("Y", "fn(X) -> Y")],
    );
}

#[test]
fn consts_used_before_definition() {
    assert_module_infer!(
        "pub fn a() { b }
        const b = 1",
        vec![("a", "fn() -> Int")],
    );
}

#[test]
fn mutual_recursion() {
    assert_module_infer!(
        "pub fn a() { b() }
         fn b() { a() }",
        vec![("a", "fn() -> a")],
    );

    assert_module_infer!(
        "pub fn a() { b() }
         fn b() { a() + 1 }",
        vec![("a", "fn() -> Int")],
    );
}

#[test]
fn type_annotations() {
    assert_module_infer!(
        "pub type Box(x) { Box(label: String, contents: x) }
         pub fn id(x: Box(y)) { x }",
        vec![
            ("Box", "fn(String, a) -> Box(a)"),
            ("id", "fn(Box(a)) -> Box(a)"),
        ],
    );

    assert_module_infer!("pub fn go(x: Int) { x }", vec![("go", "fn(Int) -> Int")],);
    assert_module_infer!("pub fn go(x: b) -> b { x }", vec![("go", "fn(a) -> a")],);
    assert_module_infer!("pub fn go(x) -> b { x }", vec![("go", "fn(a) -> a")],);
    assert_module_infer!("pub fn go(x: b) { x }", vec![("go", "fn(a) -> a")],);
    assert_module_infer!(
        "pub fn go(x: List(b)) -> List(b) { x }",
        vec![("go", "fn(List(a)) -> List(a)")],
    );
    assert_module_infer!(
        "pub fn go(x: List(b)) { x }",
        vec![("go", "fn(List(a)) -> List(a)")],
    );
    assert_module_infer!(
        "pub fn go(x: List(String)) { x }",
        vec![("go", "fn(List(String)) -> List(String)")],
    );
    assert_module_infer!("pub fn go(x: b, y: c) { x }", vec![("go", "fn(a, b) -> a")],);
    assert_module_infer!("pub fn go(x) -> Int { x }", vec![("go", "fn(Int) -> Int")],);

    assert_module_infer!(
        "pub fn id(x: x) { x }
         pub fn float() { id(1.0) }
         pub fn int() { id(1) }",
        vec![
            ("float", "fn() -> Float"),
            ("id", "fn(a) -> a"),
            ("int", "fn() -> Int"),
        ],
    );
}

#[test]
fn early_function_generalisation() {
    assert_module_infer!(
        "pub fn id(x) { x }
         pub fn int() { id(1) }",
        vec![("id", "fn(a) -> a"), ("int", "fn() -> Int")],
    );

    assert_module_infer!(
        "pub fn id(x) { x }
         pub fn int() { id(1) }
         pub fn float() { id(1.0) }
         ",
        vec![
            ("float", "fn() -> Float"),
            ("id", "fn(a) -> a"),
            ("int", "fn() -> Int"),
        ],
    );
}

// https://github.com/gleam-lang/gleam/issues/970
#[test]
fn bitstring_pattern_unification() {
    assert_module_infer!(
        "pub fn m(x) { case x { <<>> -> Nil _ -> Nil} }",
        vec![("m", "fn(BitString) -> Nil")],
    );

    assert_module_infer!(
        "pub fn m(x) { case x { <<>> -> Nil _ -> Nil} }",
        vec![("m", "fn(BitString) -> Nil")],
    );
}

// https://github.com/gleam-lang/gleam/issues/983
#[test]
fn qualified_prelude() {
    assert_module_infer!(
        "import gleam
pub fn a() {
  gleam.Ok(1)
}",
        vec![("a", "fn() -> Result(Int, a)")],
    );
}

// https://github.com/gleam-lang/gleam/issues/1029
#[test]
fn empty_list_const() {
    assert_module_infer!(
        "pub const empty = []
pub fn a() {
    empty
}",
        vec![("a", "fn() -> List(a)"), ("empty", "List(a)")],
    );
}

#[test]
fn let_as_expression() {
    assert_infer!("let x = 1", "Int");
}

#[test]
fn let_as_expression1() {
    assert_infer!("let x = { let x = 1 }", "Int");
}

#[test]
fn let_as_expression2() {
    assert_infer!("let x = { let x = 1. }", "Float");
}

#[test]
fn string_concat_ok() {
    assert_infer!(r#" "1" <> "2" "#, "String");
}

#[test]
fn string_concat_ko_1() {
    assert_error!(r#" "1" <> 2 "#);
}

#[test]
fn string_concat_ko_2() {
    assert_error!(r#" 1 <> "2" "#);
}

// https://github.com/gleam-lang/gleam/issues/1087
#[test]
fn generic_inner_access() {
    assert_module_infer!(
        "pub type B(b) { B(value: b) }
pub fn b_get_first(b: B(#(a))) {
  b.value.0
}",
        vec![("B", "fn(a) -> B(a)"), ("b_get_first", "fn(B(#(a))) -> a")],
    );
}

// https://github.com/gleam-lang/gleam/issues/1093
#[test]
fn fn_contextual_info() {
    assert_module_infer!(
        "
type Box {
  Box(inner: Int)
}

fn call(argument: t, function: fn(t) -> tt) -> tt {
  function(argument)
}

pub fn main() {
  call(Box(1), fn(box) { box.inner })
}
",
        vec![("main", "fn() -> Int")],
    );
}

// https://github.com/gleam-lang/gleam/issues/1519
#[test]
fn permit_holes_in_fn_args_and_returns() {
    assert_module_infer!(
        "pub fn run(args: List(_)) -> List(_) {
  todo
}",
        vec![("run", "fn(List(a)) -> List(b)")],
    );
}

// Rattard's parser issue
#[test]
fn block_maths() {
    assert_module_infer!(
        "pub fn do(max, min) {
  { max -. min } /. { max +. min }
}",
        vec![("do", "fn(Float, Float) -> Float")],
    );
}
