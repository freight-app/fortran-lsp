use std::path::{Path, PathBuf};

use crate::{
    semantic_token_type, GenericBindingKind, ParsedFile, Position, PreprocessorKind, RenameError,
    SemanticToken, SymbolKind, Visibility, Workspace,
};

#[test]
fn parses_modules_subroutines_functions_and_vars() {
    let src = r#"
module math
  implicit none
  integer, parameter :: rk = 8
contains
  subroutine axpy(a, x, y)
    real, intent(in) :: a
    real, intent(in) :: x
    real, intent(inout) :: y
  end subroutine axpy
end module math
"#;
    let parsed = ParsedFile::parse("math.f90", src);
    let names: Vec<_> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"math"));
    assert!(names.contains(&"axpy"));
    assert!(names.contains(&"rk"));
    assert!(names.contains(&"a"));
    let axpy = parsed.symbols.iter().find(|s| s.name == "axpy").unwrap();
    assert_eq!(axpy.kind, SymbolKind::Subroutine);
    assert_eq!(axpy.args, vec!["a", "x", "y"]);
    assert_eq!(axpy.scope, vec!["math"]);
}

#[test]
fn parses_type_prefixed_functions() {
    let src = "module linalg\n\
contains\n\
logical function solve2(a, b, x)\n\
real :: a\n\
real :: b\n\
real :: x\n\
solve2 = .true.\n\
end function solve2\n\
end module";
    let parsed = ParsedFile::parse("linalg.f90", src);
    let solve2 = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "solve2")
        .expect("typed function should be indexed");
    assert_eq!(solve2.kind, SymbolKind::Function);
    assert_eq!(solve2.args, vec!["a", "b", "x"]);
    assert_eq!(solve2.scope, vec!["linalg"]);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn parses_labeled_blocks_and_select_case_constructs() {
    let src = "subroutine f(n)\n\
main : block\n\
select case (n)\n\
case (1)\n\
n = n + 1\n\
end select\n\
end block main\n\
end subroutine";
    let parsed = ParsedFile::parse("constructs.f90", src);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.kind == SymbolKind::Block));
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.kind == SymbolKind::SelectType));
}

#[test]
fn declaration_array_constructors_do_not_create_duplicate_symbols() {
    let src = "subroutine fcn()\n\
real, parameter :: y(3) = [1.0, 2.0, 3.0]\n\
end subroutine";
    let parsed = ParsedFile::parse("arrays.f90", src);
    let y_symbols: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|sym| sym.name == "y")
        .collect();
    assert_eq!(y_symbols.len(), 1);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn legacy_declarations_without_double_colon_are_indexed() {
    let src = "program p\n\
integer, parameter :: rk = kind(1.0d0)\n\
complex(rk) f_hat(0:8)\n\
end program";
    let parsed = ParsedFile::parse("legacy.f90", src);
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| { sym.name == "f_hat" && sym.type_spec.as_deref() == Some("complex(rk)") }));
}

#[test]
fn procedure_declarations_outside_types_are_not_type_bound_methods() {
    let src = "module fit\n\
abstract interface\n\
function expr_f(x) result(y)\n\
real, intent(in) :: x\n\
real :: y\n\
end function\n\
end interface\n\
contains\n\
subroutine find_fit(expr)\n\
procedure(expr_f) :: expr\n\
end subroutine\n\
end module";
    let parsed = ParsedFile::parse("fit.f90", src);
    let expr = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "expr")
        .expect("procedure dummy should be indexed");
    assert_eq!(expr.kind, SymbolKind::Variable);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn workspace_resolves_use_only_definition() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\ncontains\nsubroutine axpy()\nend subroutine\nend module",
    );
    let app = "program app\nuse math, only: axpy\ncall axpy()\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let sym = ws
        .definition(Path::new("app.f90"), Position::new(2, 6), app)
        .unwrap();
    assert_eq!(sym.name, "axpy");
    assert_eq!(sym.scope, vec!["math"]);
    let loc = ws
        .definition_location(Path::new("app.f90"), Position::new(2, 6), app)
        .unwrap();
    assert_eq!(loc.file, PathBuf::from("math.f90"));
    assert_eq!(loc.range.start.line, 2);
}

#[test]
fn workspace_symbols_search_indexed_files() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\ncontains\nsubroutine axpy()\nend subroutine\nend module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\ninteger :: alpha\nend program",
    );

    let matches = ws.workspace_symbols("ax");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].qualified_name(), "math::axpy");

    let all = ws.workspace_symbols("");
    assert!(all.iter().any(|sym| sym.qualified_name() == "app::alpha"));
    assert!(all.iter().any(|sym| sym.qualified_name() == "math"));
}

#[test]
fn workspace_upsert_skips_unchanged_source() {
    let mut ws = Workspace::new();
    let path = PathBuf::from("math.f90");
    let first = "module math\nend module";
    let second = "module stats\nend module";

    assert!(ws.upsert_file(path.clone(), first));
    assert!(!ws.upsert_file(path.clone(), first));
    assert_eq!(ws.workspace_symbols("math").len(), 1);

    assert!(ws.upsert_file(path, second));
    assert!(ws.workspace_symbols("math").is_empty());
    assert_eq!(ws.workspace_symbols("stats").len(), 1);
}

#[test]
fn line_length_diagnostics_follow_fortls_limits() {
    let mut ws = Workspace::new();
    ws.set_line_length_limits(Some(12), Some(10));
    let src = "program app\ninteger :: long_name\n! comment that is too long\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(
        diagnostics[0].message,
        "Line length exceeds \"max_line_length\" (12)"
    );
    assert_eq!(diagnostics[0].range.start, Position::new(1, 12));
    assert_eq!(diagnostics[0].range.end, Position::new(1, 20));
    assert_eq!(
        diagnostics[1].message,
        "Comment line length exceeds \"max_comment_line_length\" (10)"
    );
    assert_eq!(diagnostics[1].range.start, Position::new(2, 10));
}

#[test]
fn line_length_diagnostics_handle_fixed_form_comments() {
    let mut ws = Workspace::new();
    ws.set_line_length_limits(Some(15), Some(6));
    let src = "      program p\nC fixed comment\n      integer longname\n      end";
    ws.upsert_file(PathBuf::from("legacy.f"), src);

    let diagnostics = ws.diagnostics(Path::new("legacy.f"));
    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics[0].message.contains("max_comment_line_length"));
    assert_eq!(diagnostics[0].range.start, Position::new(1, 6));
    assert!(diagnostics[1].message.contains("max_line_length"));
    assert_eq!(diagnostics[1].range.start, Position::new(2, 15));
}

#[test]
fn unresolved_kind_selectors_are_diagnosed() {
    let mut ws = Workspace::new();
    let src = "subroutine sort(array)\n\
integer(kind=int_index), intent(in) :: array(:)\n\
integer :: n\n\
n = size(array, kind=int_index)\n\
end subroutine";
    ws.upsert_file(PathBuf::from("sort.f90"), src);

    let diagnostics = ws.diagnostics(Path::new("sort.f90"));
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diag| diag.message.as_str())
        .collect();
    assert_eq!(messages, vec!["object \"int_index\" not found in scope"]);
    assert_eq!(diagnostics[0].range.start.line, 1);
}

#[test]
fn available_kind_selectors_do_not_report() {
    let mut ws = Workspace::new();
    let src = "module kinds\n\
use iso_fortran_env, only: int32\n\
implicit none\n\
integer, parameter :: int_index = int32\n\
contains\n\
subroutine sort(array)\n\
integer(kind=int_index), intent(in) :: array(:)\n\
integer :: n\n\
n = size(array, kind=int_index)\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("sort.f90"), src);

    let diagnostics = ws.diagnostics(Path::new("sort.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn unresolved_use_suppresses_kind_selector_cascades() {
    let mut ws = Workspace::new();
    let src = "subroutine fft(x)\n\
use fftpack_kind, only: dp\n\
real(kind=dp), intent(in) :: x(:)\n\
end subroutine";
    ws.upsert_file(PathBuf::from("fft.f90"), src);

    let diagnostics = ws.diagnostics(Path::new("fft.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].message,
        "module `fftpack_kind` could not be resolved"
    );
}

#[test]
fn unresolved_use_does_not_suppress_shorthand_kind_selector() {
    let mut ws = Workspace::new();
    let src = "program app\n\
use stdlib_kinds, only: dp\n\
implicit none\n\
real(dp), allocatable :: indices(:)\n\
end program";
    ws.upsert_file(PathBuf::from("app.f90"), src);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    let messages: Vec<_> = diagnostics
        .iter()
        .map(|diag| diag.message.as_str())
        .collect();
    assert_eq!(
        messages,
        vec![
            "module `stdlib_kinds` could not be resolved",
            "object \"dp\" not found in scope",
        ]
    );
}

#[test]
fn include_parameters_satisfy_shorthand_kind_selector() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("pkg/inc/parameters.f90"),
        "integer, parameter :: dp = 8",
    );
    ws.upsert_file(
        PathBuf::from("pkg/src/lib.f90"),
        "module lib\n\
include \"parameters.f90\"\n\
real(dp) :: value\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("pkg/src/lib.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("object \"dp\"")),
        "{diagnostics:?}"
    );
}

#[test]
fn unresolved_whole_use_suppresses_shorthand_kind_selector() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use netcdf4_f03\n\
integer(c_int) :: ncid\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "module `netcdf4_f03` could not be resolved"
    );
}

#[test]
fn submodule_kind_selectors_see_ancestor_uses() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("parent.f90"),
        "module parent\n\
use iso_fortran_env, only: int32\n\
interface\n\
module subroutine sort(array)\n\
integer(kind=int32), intent(in) :: array(:)\n\
end subroutine\n\
end interface\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("child.f90"),
        "submodule(parent) child\n\
contains\n\
module procedure sort\n\
integer(kind=int32) :: n\n\
n = size(array, kind=int32)\n\
end procedure\n\
end submodule",
    );

    let diagnostics = ws.diagnostics(Path::new("child.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn submodule_kind_selectors_see_ancestor_parameters() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("parent.f90"),
        "module parent\n\
integer, parameter, public :: int_index = 4\n\
interface\n\
module subroutine sort(array)\n\
integer(kind=int_index), intent(in) :: array(:)\n\
end subroutine\n\
end interface\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("child.f90"),
        "submodule(parent) child\n\
contains\n\
module procedure sort\n\
integer(kind=int_index) :: n\n\
n = size(array, kind=int_index)\n\
end procedure\n\
end submodule",
    );

    let diagnostics = ws.diagnostics(Path::new("child.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn submodule_kind_selectors_suppress_unresolved_ancestor_uses() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("parent.f90"),
        "module parent\n\
use kind_mod, only: rk\n\
public :: rk\n\
interface\n\
module subroutine transform(x)\n\
real(kind=rk), intent(in) :: x(:)\n\
end subroutine\n\
end interface\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("child.f90"),
        "submodule(parent) child\n\
contains\n\
module procedure transform\n\
real(kind=rk) :: y\n\
end procedure\n\
end submodule",
    );

    let diagnostics = ws.diagnostics(Path::new("child.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn resolved_module_unresolved_reexports_suppress_kind_selector_cascades() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\n\
use missing_hashes\n\
public :: int_hash\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("consumer.f90"),
        "module consumer\n\
use wrapper, only: int_hash\n\
integer(int_hash) :: value\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("consumer.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("object \"int_hash\"")),
        "{diagnostics:?}"
    );
    assert!(diagnostics
        .iter()
        .any(|diag| diag.message.contains("module `wrapper`")));
}

#[test]
fn records_and_resolves_use_renames() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\ncontains\nsubroutine axpy()\nend subroutine\nend module",
    );
    let app = "program app\nuse math, only: saxpy => axpy\ncall saxpy()\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let parsed = ws.file(Path::new("app.f90")).unwrap();
    assert_eq!(parsed.uses[0].only, vec!["saxpy"]);
    assert_eq!(parsed.uses[0].renames[0].local, "saxpy");
    assert_eq!(parsed.uses[0].renames[0].remote, "axpy");
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
    let sym = ws
        .definition(Path::new("app.f90"), Position::new(2, 6), app)
        .unwrap();
    assert_eq!(sym.name, "axpy");
    let completions = ws.completions(Path::new("app.f90"), "sa");
    assert!(completions.iter().any(|item| item.label == "saxpy"));
}

#[test]
fn parses_submodules_and_links_module_procedure_to_ancestor_interface() {
    let mut ws = Workspace::new();
    let module = "module math\n\
interface\n\
!! Scale and add vectors.\n\
module subroutine axpy(a, x, y)\n\
real :: a\n\
real :: x\n\
real :: y\n\
end subroutine\n\
end interface\n\
end module";
    let submodule = "submodule (math) math_impl\n\
contains\n\
module procedure axpy\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("math.f90"), module);
    ws.upsert_file(PathBuf::from("math_impl.f90"), submodule);

    let parsed = ws.file(Path::new("math_impl.f90")).unwrap();
    let submod = parsed
        .symbols
        .iter()
        .find(|sym| sym.kind == SymbolKind::Submodule)
        .unwrap();
    assert_eq!(submod.name, "math_impl");
    assert_eq!(submod.ancestor.as_deref(), Some("math"));
    let implementation = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "axpy" && sym.is_module_procedure)
        .unwrap();
    assert_eq!(implementation.scope, vec!["math_impl"]);

    let hover = ws
        .hover(Path::new("math_impl.f90"), Position::new(2, 18), submodule)
        .unwrap();
    assert!(hover.contains("module subroutine axpy(a, x, y)"));
    assert!(hover.contains("Scale and add vectors."));
    let definition = ws
        .definition(Path::new("math_impl.f90"), Position::new(2, 18), submodule)
        .unwrap();
    assert_eq!(definition.file, PathBuf::from("math.f90"));
    assert_eq!(definition.args, vec!["a", "x", "y"]);
    let implementation = ws
        .implementation_location(Path::new("math.f90"), Position::new(3, 18), module)
        .unwrap();
    assert_eq!(implementation.file, PathBuf::from("math_impl.f90"));
    assert_eq!(implementation.range.start, Position::new(2, 17));
    assert!(ws.diagnostics(Path::new("math_impl.f90")).is_empty());
}

#[test]
fn used_module_procedure_prototypes_are_visible() {
    let mut ws = Workspace::new();
    let module = "module math\n\
interface\n\
module subroutine axpy(a, x, y)\n\
real :: a\n\
real :: x\n\
real :: y\n\
end subroutine\n\
end interface\n\
end module";
    let app = "program app\n\
use math, only: axpy\n\
real :: a, x, y\n\
call axpy(a, x, y)\n\
end program";
    ws.upsert_file(PathBuf::from("math.f90"), module);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let hover = ws
        .hover(Path::new("app.f90"), Position::new(3, 6), app)
        .unwrap();
    assert!(hover.contains("module subroutine axpy(a, x, y)"));
    let definition = ws
        .definition_location(Path::new("app.f90"), Position::new(3, 6), app)
        .unwrap();
    assert_eq!(definition.file, PathBuf::from("math.f90"));
    assert_eq!(definition.range.start, Position::new(2, 18));
    let signature = ws
        .signature_help(Path::new("app.f90"), Position::new(3, 10), app)
        .unwrap();
    assert_eq!(signature.label, "axpy(a, x, y)");
    assert_eq!(signature.parameters, vec!["a", "x", "y"]);
    let refs = ws.references(Path::new("app.f90"), Position::new(3, 6), app);
    assert!(refs.iter().any(
        |loc| loc.file == PathBuf::from("math.f90") && loc.range.start == Position::new(2, 18)
    ));
}

#[test]
fn unresolved_submodule_ancestors_do_not_cascade_diagnostics() {
    let mut ws = Workspace::new();
    let submodule = "submodule (missing_parent) math_impl\n\
contains\n\
module procedure axpy\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("math_impl.f90"), submodule);
    let diagnostics = ws.diagnostics(Path::new("math_impl.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn reports_module_procedures_missing_from_resolved_ancestor_interfaces() {
    let mut ws = Workspace::new();
    let module = "module parent\n\
interface\n\
module subroutine other()\n\
end subroutine other\n\
end interface\n\
end module";
    let submodule = "submodule (parent) math_impl\n\
contains\n\
module procedure axpy\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("parent.f90"), module);
    ws.upsert_file(PathBuf::from("math_impl.f90"), submodule);
    let diagnostics = ws.diagnostics(Path::new("math_impl.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("module procedure `axpy`"));
}

#[test]
fn submodule_implementations_see_private_ancestor_types() {
    let mut ws = Workspace::new();
    let module = "module parent\n\
private\n\
type :: known\n\
end type\n\
interface\n\
module function make_known() result(value)\n\
type(known) :: value\n\
end function make_known\n\
end interface\n\
end module";
    let submodule = "submodule (parent) parent_impl\n\
contains\n\
module procedure make_known\n\
type(known) :: value\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("parent.f90"), module);
    ws.upsert_file(PathBuf::from("parent_impl.f90"), submodule);
    let diagnostics = ws.diagnostics(Path::new("parent_impl.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn submodule_implementations_see_types_from_ancestor_uses() {
    let mut ws = Workspace::new();
    let dependency = "module wrappers\n\
type :: key_type\n\
end type\n\
end module";
    let module = "module parent\n\
use wrappers, only: key_type\n\
interface\n\
module function make_key() result(value)\n\
type(key_type) :: value\n\
end function make_key\n\
end interface\n\
end module";
    let submodule = "submodule (parent) parent_impl\n\
contains\n\
module procedure make_key\n\
type(key_type) :: value\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("wrappers.f90"), dependency);
    ws.upsert_file(PathBuf::from("parent.f90"), module);
    ws.upsert_file(PathBuf::from("parent_impl.f90"), submodule);
    let diagnostics = ws.diagnostics(Path::new("parent_impl.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn submodule_implementations_suppress_types_from_unresolved_ancestor_uses() {
    let mut ws = Workspace::new();
    let module = "module parent\n\
use missing_strings, only: string_type\n\
interface\n\
module function make_string() result(value)\n\
type(string_type) :: value\n\
end function make_string\n\
end interface\n\
end module";
    let submodule = "submodule (parent) parent_impl\n\
contains\n\
module procedure make_string\n\
type(string_type) :: value\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("parent.f90"), module);
    ws.upsert_file(PathBuf::from("parent_impl.f90"), submodule);
    let diagnostics = ws.diagnostics(Path::new("parent_impl.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    let parent_diagnostics = ws.diagnostics(Path::new("parent.f90"));
    assert!(parent_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("missing_strings")));
}

#[test]
fn reports_module_procedures_without_ancestor_interface_prototypes() {
    let mut ws = Workspace::new();
    ws.upsert_file(PathBuf::from("math.f90"), "module math\nend module");
    let submodule = "submodule (math) math_impl\n\
contains\n\
module procedure axpy\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("math_impl.f90"), submodule);
    let diagnostics = ws.diagnostics(Path::new("math_impl.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("no matching ancestor interface"));
}

#[test]
fn reports_use_renames_to_missing_remote_exports() {
    let mut ws = Workspace::new();
    ws.upsert_file(PathBuf::from("math.f90"), "module math\nend module");
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse math, only: saxpy => axpy\nend program",
    );
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("saxpy => axpy"));
}

#[test]
fn use_only_accepts_public_interface_prototype_exports() {
    let mut ws = Workspace::new();
    let module = "module fftpack\n\
implicit none\n\
private\n\
public :: zfftf\n\
interface\n\
pure subroutine zfftf(n, c)\n\
integer, intent(in) :: n\n\
complex, intent(inout) :: c(*)\n\
end subroutine zfftf\n\
end interface\n\
end module";
    let app = "program app\n\
use fftpack, only: zfftf\n\
end program";
    ws.upsert_file(PathBuf::from("fftpack.f90"), module);
    ws.upsert_file(PathBuf::from("app.f90"), app);
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
}

#[test]
fn reports_duplicate_symbols_in_scope() {
    let parsed = ParsedFile::parse(
        "dup.f90",
        "subroutine s()\ninteger :: x\nreal :: x\nend subroutine",
    );
    assert_eq!(parsed.diagnostics.len(), 1);
    assert!(parsed.diagnostics[0].message.contains("already defined"));
}

#[test]
fn allows_repeated_anonymous_interface_blocks() {
    let parsed = ParsedFile::parse(
        "interfaces.f90",
        "module m\n\
interface\n\
module procedure first\n\
end interface\n\
interface\n\
module procedure second\n\
end interface\n\
contains\n\
subroutine first()\n\
end subroutine\n\
subroutine second()\n\
end subroutine\n\
end module",
    );
    assert!(parsed
        .diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("already defined")));
}

#[test]
fn allows_generic_constructor_interface_named_like_type() {
    let parsed = ParsedFile::parse(
        "constructor.f90",
        "module m\n\
type vector\n\
integer :: x\n\
end type\n\
interface vector\n\
module procedure new_vector\n\
end interface\n\
contains\n\
function new_vector(x) result(value)\n\
integer :: x\n\
type(vector) :: value\n\
end function\n\
end module",
    );
    assert!(parsed
        .diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("already defined")));
}

#[test]
fn allows_generic_interface_link_named_like_implementation() {
    let parsed = ParsedFile::parse(
        "same_name_generic.f90",
        "module strings\n\
interface f_string\n\
module procedure f_string, f_string_cptr\n\
end interface\n\
contains\n\
function f_string(c_string)\n\
character(len=1), intent(in) :: c_string(*)\n\
character(:), allocatable :: f_string\n\
end function\n\
function f_string_cptr(cptr) result(s)\n\
integer, intent(in) :: cptr\n\
character(:), allocatable :: s\n\
end function\n\
end module",
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("already defined")),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn reports_variables_that_mask_parent_scope_variables() {
    let parsed = ParsedFile::parse(
        "mask.f90",
        "module m\n\
integer :: value\n\
contains\n\
subroutine work()\n\
integer :: value\n\
integer :: block_value\n\
block\n\
integer :: block_value\n\
end block\n\
end subroutine\n\
end module",
    );
    let masking: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 2);
    assert!(masking
        .iter()
        .all(|diag| diag.severity == crate::DiagnosticSeverity::Warning));
}

#[test]
fn reports_locals_that_mask_module_callables_and_interfaces() {
    let parsed = ParsedFile::parse(
        "mask_callable.f90",
        "module strings\n\
interface str\n\
module procedure str_int\n\
end interface\n\
contains\n\
function lower(text) result(out)\n\
character(len=*), intent(in) :: text\n\
character(len=:), allocatable :: out\n\
end function\n\
function is_name(line) result(ok)\n\
character(len=*), parameter :: lower = 'abcdefghijklmnopqrstuvwxyz'\n\
character(len=*), intent(in) :: line\n\
logical :: ok\n\
end function\n\
function str_int(i) result(s)\n\
integer, intent(in) :: i\n\
character(len=8) :: s\n\
end function\n\
function join(str) result(out)\n\
character(len=*), intent(in) :: str(:)\n\
character(len=:), allocatable :: out\n\
end function\n\
end module",
    );
    let masking: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 2, "{:?}", parsed.diagnostics);
    for name in ["lower", "str"] {
        assert!(
            masking
                .iter()
                .any(|diag| diag.message.contains(&format!("\"{name}\""))),
            "{:?}",
            parsed.diagnostics
        );
    }
}

#[test]
fn reports_locals_that_mask_later_module_callables() {
    let mut ws = Workspace::new();
    let src = "module m\n\
implicit none\n\
contains\n\
subroutine before()\n\
integer :: later\n\
end subroutine\n\
function later() result(value)\n\
integer :: value\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("later_mask.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("later_mask.f90"));
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"later\" masks variable in parent scope"),
        "{diagnostics:?}"
    );
}

#[test]
fn reports_dummies_that_mask_case_insensitive_module_functions() {
    let parsed = ParsedFile::parse(
        "mask_case.f90",
        "module env\n\
contains\n\
function OS_NAME(os)\n\
integer, intent(in) :: os\n\
character(len=:), allocatable :: OS_NAME\n\
end function\n\
function match_os_type(os_name) result(os_type)\n\
character(len=*), intent(in) :: os_name\n\
integer :: os_type\n\
end function\n\
end module",
    );
    let masking: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert!(
        masking
            .iter()
            .any(|diag| diag.message.contains("\"os_name\"")),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn function_implicit_results_do_not_mask_their_own_module_procedure() {
    let parsed = ParsedFile::parse(
        "own_result.f90",
        "module filesystem\n\
contains\n\
function glob(pattern)\n\
character(len=*), intent(in) :: pattern\n\
logical :: glob\n\
end function\n\
function caller(glob)\n\
logical, intent(in) :: glob\n\
logical :: caller\n\
end function\n\
end module",
    );
    let masking: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 1, "{:?}", parsed.diagnostics);
    assert!(masking[0].message.contains("\"glob\""));
}

#[test]
fn reports_procedure_names_that_mask_type_members() {
    let parsed = ParsedFile::parse(
        "mask_function.f90",
        "module m\n\
type :: logger\n\
logical :: time_stamp\n\
end type\n\
contains\n\
function time_stamp()\n\
character(23) :: time_stamp\n\
end function\n\
end module",
    );
    let masking: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 2, "{:?}", parsed.diagnostics);
}

#[test]
fn reports_procedure_locals_that_mask_type_bound_methods() {
    let parsed = ParsedFile::parse(
        "mask_method.f90",
        "module m\n\
type, abstract :: hashmap\n\
contains\n\
procedure(depth_iface), deferred :: total_depth\n\
procedure :: slots_bits\n\
end type\n\
abstract interface\n\
function depth_iface(map) result(total_depth)\n\
import hashmap\n\
class(hashmap), intent(in) :: map\n\
integer :: total_depth\n\
end function\n\
end interface\n\
interface\n\
module function total_open_depth(map) result(total_depth)\n\
class(hashmap), intent(in) :: map\n\
integer :: total_depth\n\
end function\n\
end interface\n\
contains\n\
subroutine init(slots_bits)\n\
integer, intent(in) :: slots_bits\n\
end subroutine\n\
function total_impl(map) result(total_depth)\n\
class(hashmap), intent(in) :: map\n\
integer :: total_depth\n\
end function\n\
function slots_bits(map)\n\
class(hashmap), intent(in) :: map\n\
integer :: slots_bits\n\
end function\n\
end module",
    );
    let masking: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 3, "{:?}", parsed.diagnostics);
    assert!(masking
        .iter()
        .all(|diag| diag.severity == crate::DiagnosticSeverity::Warning));
}

#[test]
fn abstract_interface_dummy_args_do_not_mask_type_bound_methods() {
    let parsed = ParsedFile::parse(
        "abstract_interface_dummy_mask.f90",
        "module m\n\
type :: process_type\n\
contains\n\
procedure :: pid => process_get_id\n\
procedure :: slots_bits\n\
end type\n\
abstract interface\n\
subroutine process_callback(pid, slots_bits)\n\
integer, intent(in) :: pid\n\
integer, intent(in) :: slots_bits\n\
end subroutine\n\
end interface\n\
contains\n\
integer function process_get_id(process)\n\
type(process_type), intent(in) :: process\n\
process_get_id = 0\n\
end function\n\
integer function slots_bits(process)\n\
type(process_type), intent(in) :: process\n\
slots_bits = 0\n\
end function\n\
end module",
    );
    let masking: Vec<_> = parsed
        .diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 1, "{:?}", parsed.diagnostics);
    assert!(
        masking
            .iter()
            .any(|diag| diag.message.contains("slots_bits")),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn aliased_type_bound_result_names_do_not_mask_binding_names() {
    let parsed = ParsedFile::parse(
        "aliased_result_mask.f90",
        "module m\n\
type :: platform_config_t\n\
contains\n\
procedure :: compiler_name => platform_compiler_name\n\
procedure :: os_name => platform_os_name\n\
procedure :: name => platform_config_name\n\
end type\n\
contains\n\
function platform_compiler_name(self) result(name)\n\
class(platform_config_t), intent(in) :: self\n\
character(len=:), allocatable :: name\n\
end function\n\
function platform_os_name(self) result(name)\n\
class(platform_config_t), intent(in) :: self\n\
character(len=:), allocatable :: name\n\
end function\n\
function platform_config_name(self) result(name)\n\
class(platform_config_t), intent(in) :: self\n\
character(len=:), allocatable :: name\n\
end function\n\
function compiler_id_name(id) result(name)\n\
integer, intent(in) :: id\n\
character(len=:), allocatable :: name\n\
end function\n\
end module",
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|diag| !diag.message.contains("masks variable in parent scope")),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn constructor_interface_procedures_do_not_mask_type_members() {
    let parsed = ParsedFile::parse(
        "constructor_interface_mask.f90",
        "module m\n\
type :: build_progress_t\n\
integer :: n_target\n\
end type\n\
interface build_progress_t\n\
procedure :: new_build_progress\n\
end interface\n\
contains\n\
function new_build_progress() result(progress)\n\
type(build_progress_t) :: progress\n\
end function\n\
end module",
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|diag| !diag.message.eq_ignore_ascii_case(
                "variable \"new_build_progress\" masks variable in parent scope"
            )),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn constructor_interface_dummies_do_not_mask_same_named_type_members() {
    let parsed = ParsedFile::parse(
        "constructor_dummy_mask.f90",
        "module layers\n\
type :: conv1d_layer\n\
integer :: filters\n\
integer :: stride\n\
end type\n\
interface conv1d_layer\n\
module function conv1d_layer_cons(filters, stride) result(res)\n\
integer, intent(in) :: filters\n\
integer, intent(in) :: stride\n\
type(conv1d_layer) :: res\n\
end function\n\
end interface\n\
end module",
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|diag| !diag.message.contains("masks variable in parent scope")),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn dummies_do_not_mask_aliased_type_bound_binding_names() {
    let parsed = ParsedFile::parse(
        "aliased_binding_dummy_mask.f90",
        "module m\n\
type :: compiler_t\n\
contains\n\
procedure :: name => compiler_name\n\
end type\n\
contains\n\
subroutine write_response_file(name, argv)\n\
character(len=*), intent(in) :: name\n\
character(len=*), intent(in) :: argv\n\
end subroutine\n\
function compiler_name(self) result(name)\n\
class(compiler_t), intent(in) :: self\n\
character(len=:), allocatable :: name\n\
end function\n\
end module",
    );
    assert!(
        parsed.diagnostics.iter().all(|diag| !diag
            .message
            .eq_ignore_ascii_case("variable \"name\" masks variable in parent scope")),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn locals_mask_parent_use_only_names_case_insensitively() {
    let parsed = ParsedFile::parse(
        "feature.f90",
        "module feature\n\
use env, only: OS_NAME\n\
contains\n\
subroutine new_feature()\n\
character(len=:), allocatable :: os_name\n\
end subroutine\n\
end module",
    );

    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"os_name\" masks variable in parent scope"),
        "{:?}",
        parsed.diagnostics
    );
}

#[test]
fn contained_function_dummies_mask_program_parameters() {
    let source = "program regrid\n\
integer, parameter :: nx = 2, ny = 2\n\
integer, parameter :: wp = kind(1.0)\n\
real(wp),dimension(nx),parameter :: x = [0.0_wp,2.0_wp]\n\
real(wp),dimension(ny),parameter :: y = [0.0_wp,2.0_wp]\n\
integer :: i\n\
do i = 1, nx\n\
print *, x(i), y(i)\n\
end do\n\
contains\n\
function test_func(x,y) result(f)\n\
real :: f\n\
real, intent(in) :: x, y\n\
f = x + y\n\
end function\n\
end program";
    let parsed = ParsedFile::parse("regrid.f90", source);

    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"x\" masks variable in parent scope"),
        "{:?}",
        parsed.diagnostics
    );
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"y\" masks variable in parent scope"),
        "{:?}",
        parsed.diagnostics
    );

    let mut ws = Workspace::new();
    ws.upsert_file(PathBuf::from("regrid.f90"), source);
    let diagnostics = ws.diagnostics(Path::new("regrid.f90"));
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"x\" masks variable in parent scope"),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"y\" masks variable in parent scope"),
        "{diagnostics:?}"
    );
}

#[test]
fn contained_function_results_mask_parent_variables() {
    let mut ws = Workspace::new();
    let source = "program optimizers\n\
logical :: converged = .false.\n\
contains\n\
pure logical function check_convergence() result(converged)\n\
converged = .true.\n\
end function\n\
end program";
    ws.upsert_file(PathBuf::from("optimizers.f90"), source);

    let diagnostics = ws.diagnostics(Path::new("optimizers.f90"));
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"converged\" masks variable in parent scope"),
        "{diagnostics:?}"
    );
}

#[test]
fn debug_bspline_workspace_regrid_after_all_files() {
    let root = Path::new("/tmp/freight-bspline-fixture");
    let mut files = Vec::new();
    for dir in ["src", "test"] {
        for entry in std::fs::read_dir(root.join(dir)).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("f90"))
            {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut ws = Workspace::new();
    for path in &files {
        let source = std::fs::read_to_string(path).unwrap();
        ws.upsert_file(path.clone(), &source);
    }
    let target = root.join("test/test_regrid.f90");
    eprintln!("{:?}", ws.diagnostics(&target));
}

#[test]
fn local_parameters_mask_names_from_whole_module_reexports() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("conversion.f90"),
        "module conversion\n\
implicit none\n\
public\n\
real, parameter :: day2sec = 86400.0\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("api.f90"),
        "module api\n\
use conversion\n\
implicit none\n\
public\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "module app\n\
use api\n\
implicit none\n\
contains\n\
subroutine generate()\n\
real, parameter :: day2sec = 86400.0\n\
end subroutine\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"day2sec\" masks variable in parent scope"),
        "{diagnostics:?}"
    );
}

#[test]
fn locals_mask_parameters_from_whole_module_imports() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("kinds.f90"),
        "module kinds\ninteger, parameter :: ip = 4\nend module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "module app\n\
use kinds\n\
contains\n\
subroutine run()\n\
integer :: ip\n\
end subroutine\n\
end module",
    );
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "Variable \"ip\" masks variable in parent scope"),
        "{diagnostics:?}"
    );
}

#[test]
fn submodule_locals_mask_ancestor_interface_and_prototype_names() {
    let mut ws = Workspace::new();
    let module = "module system\n\
type :: process_type\n\
contains\n\
procedure :: elapsed => process_lifetime\n\
end type\n\
interface elapsed\n\
module function process_lifetime(process) result(delta_t)\n\
type(process_type), intent(in) :: process\n\
real :: delta_t\n\
end function\n\
end interface\n\
interface run\n\
module function run_cmd() result(process)\n\
type(process_type) :: process\n\
end function\n\
end interface\n\
interface wait\n\
module subroutine wait_for_completion(process)\n\
type(process_type), intent(inout) :: process\n\
end subroutine\n\
end interface\n\
end module";
    let submodule = "submodule (system) system_impl\n\
contains\n\
module function process_lifetime(process) result(delta_t)\n\
type(process_type), intent(in) :: process\n\
real :: elapsed\n\
end function\n\
module function run_cmd() result(process)\n\
type(process_type) :: process\n\
end function\n\
module subroutine wait_for_completion(process)\n\
type(process_type), intent(inout) :: process\n\
logical :: wait\n\
end subroutine\n\
end submodule";
    ws.upsert_file(PathBuf::from("system.f90"), module);
    ws.upsert_file(PathBuf::from("system_impl.f90"), submodule);

    let diagnostics = ws.diagnostics(Path::new("system_impl.f90"));
    let masking: Vec<_> = diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 3, "{diagnostics:?}");
    for name in ["process", "elapsed", "wait"] {
        assert!(
            masking
                .iter()
                .any(|diag| diag.message.eq_ignore_ascii_case(&format!(
                    "variable \"{name}\" masks variable in parent scope"
                ))),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn submodule_repeated_sibling_dummies_do_not_mask_without_ancestor_name() {
    let mut ws = Workspace::new();
    let module = "module parent\n\
interface\n\
module subroutine first()\n\
end subroutine\n\
module subroutine second()\n\
end subroutine\n\
end interface\n\
end module";
    let submodule = "submodule (parent) child\n\
contains\n\
module subroutine first(arg)\n\
integer :: arg\n\
end subroutine\n\
module subroutine second(arg)\n\
integer :: arg\n\
end subroutine\n\
end submodule";
    ws.upsert_file(PathBuf::from("parent.f90"), module);
    ws.upsert_file(PathBuf::from("child.f90"), submodule);

    let diagnostics = ws.diagnostics(Path::new("child.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("masks variable in parent scope")),
        "{diagnostics:?}"
    );
}

#[test]
fn submodule_constructor_dummies_and_results_do_not_mask_ancestor_prototype_names() {
    let mut ws = Workspace::new();
    let module = "module layers\n\
type :: conv1d_layer\n\
integer :: filters\n\
integer :: stride\n\
end type\n\
interface conv1d_layer\n\
module function conv1d_layer_cons(filters, stride) result(res)\n\
integer, intent(in) :: filters\n\
integer, intent(in) :: stride\n\
type(conv1d_layer) :: res\n\
end function\n\
end interface\n\
end module";
    let submodule = "submodule(layers) layers_impl\n\
contains\n\
module function conv1d_layer_cons(filters, stride) result(res)\n\
integer, intent(in) :: filters\n\
integer, intent(in) :: stride\n\
type(conv1d_layer) :: res\n\
res % filters = filters\n\
res % stride = stride\n\
end function\n\
end submodule";
    ws.upsert_file(PathBuf::from("layers.f90"), module);
    ws.upsert_file(PathBuf::from("layers_impl.f90"), submodule);

    let diagnostics = ws.diagnostics(Path::new("layers_impl.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("masks variable in parent scope")),
        "{diagnostics:?}"
    );
}

#[test]
fn submodule_repeated_clock_locals_mask_but_dummies_do_not() {
    let mut ws = Workspace::new();
    let module = "module parent\n\
interface\n\
module subroutine first(arg)\n\
integer :: arg\n\
end subroutine\n\
module subroutine second(arg)\n\
integer :: arg\n\
end subroutine\n\
end interface\n\
end module";
    let submodule = "submodule (parent) child\n\
contains\n\
module subroutine first(arg)\n\
integer :: arg\n\
integer :: count_max\n\
end subroutine\n\
module subroutine second(arg)\n\
integer :: arg\n\
integer :: count_max\n\
end subroutine\n\
end submodule";
    ws.upsert_file(PathBuf::from("parent.f90"), module);
    ws.upsert_file(PathBuf::from("child.f90"), submodule);

    let diagnostics = ws.diagnostics(Path::new("child.f90"));
    let masking: Vec<_> = diagnostics
        .iter()
        .filter(|diag| diag.message.contains("masks variable in parent scope"))
        .collect();
    assert_eq!(masking.len(), 1, "{diagnostics:?}");
    assert!(masking[0].message.contains("count_max"), "{diagnostics:?}");
}

#[test]
fn submodule_ordinary_function_result_dummy_reports_duplicate() {
    let mut ws = Workspace::new();
    ws.upsert_file(PathBuf::from("parent.f90"), "module parent\nend module");
    ws.upsert_file(
        PathBuf::from("child.f90"),
        "submodule (parent) child\n\
contains\n\
function is_ready(is_ready) result(is_ready)\n\
logical :: is_ready\n\
end function\n\
end submodule",
    );

    let diagnostics = ws.diagnostics(Path::new("child.f90"));
    assert!(
        diagnostics.iter().any(|diag| diag
            .message
            .eq_ignore_ascii_case("variable \"is_ready\" declared twice in scope")),
        "{diagnostics:?}"
    );
}

#[test]
fn named_ancestor_function_result_dummy_reports_duplicate_in_submodule() {
    let mut ws = Workspace::new();
    let module = "module parent\n\
type :: process_type\n\
contains\n\
procedure :: is_running => process_is_running\n\
end type\n\
interface is_running\n\
logical module function process_is_running(process) result(is_running)\n\
type(process_type) :: process\n\
end function\n\
end interface\n\
interface run\n\
module function run_cmd() result(process)\n\
type(process_type) :: process\n\
end function\n\
end interface\n\
end module";
    let submodule = "submodule (parent) child\n\
contains\n\
logical module function process_is_running(process) result(is_running)\n\
type(process_type) :: process\n\
end function\n\
end submodule";
    ws.upsert_file(PathBuf::from("parent.f90"), module);
    ws.upsert_file(PathBuf::from("child.f90"), submodule);

    let diagnostics = ws.diagnostics(Path::new("child.f90"));
    assert!(
        diagnostics.iter().any(|diag| diag
            .message
            .eq_ignore_ascii_case("variable \"process\" declared twice in scope")),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics.iter().all(|diag| !diag
            .message
            .eq_ignore_ascii_case("variable \"process\" masks variable in parent scope")),
        "{diagnostics:?}"
    );
}

#[test]
fn type_members_do_not_report_parent_masking() {
    let parsed = ParsedFile::parse(
        "type_member.f90",
        "module m\ninteger :: value\ntype :: t\ninteger :: value\nend type\nend module",
    );
    assert!(!parsed
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("masks variable in parent scope")));
}

#[test]
fn reports_use_statements_after_implicit_statement() {
    let parsed = ParsedFile::parse(
        "order.f90",
        "module m\nimplicit none\nuse iso_fortran_env, only: int32\nend module",
    );
    let diagnostic = parsed
        .diagnostics
        .iter()
        .find(|diag| {
            diag.message
                .contains("USE statements after IMPLICIT statement")
        })
        .expect("expected use-after-implicit diagnostic");
    assert_eq!(diagnostic.range.start.line, 1);

    let valid = ParsedFile::parse(
        "valid.f90",
        "module m\nuse iso_fortran_env, only: int32\nimplicit none\nend module",
    );
    assert!(!valid.diagnostics.iter().any(|diag| {
        diag.message
            .contains("USE statements after IMPLICIT statement")
    }));
}

#[test]
fn reports_use_after_implicit_in_unterminated_scopes() {
    let parsed = ParsedFile::parse(
        "partial.f90",
        "module m\nimplicit none\nuse iso_fortran_env, only: int32",
    );
    assert!(parsed.diagnostics.iter().any(|diag| {
        diag.message
            .contains("USE statements after IMPLICIT statement")
    }));
    assert!(parsed
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("unterminated module scope")));
}

#[test]
fn reports_contains_statement_errors() {
    let duplicate = ParsedFile::parse(
        "dup_contains.f90",
        "module m\ncontains\ncontains\nsubroutine s()\nend subroutine\nend module",
    );
    assert!(duplicate
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("Multiple CONTAINS statements")));

    let orphan = ParsedFile::parse("orphan_contains.f90", "contains");
    assert!(orphan
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("without enclosing scope")));

    let missing = ParsedFile::parse(
        "missing_contains.f90",
        "module m\nsubroutine s()\nend subroutine\nend module",
    );
    assert!(missing.diagnostics.iter().any(|diag| {
        diag.message
            .contains("Subroutine/Function definition before CONTAINS statement")
    }));

    let valid = ParsedFile::parse(
        "valid_contains.f90",
        "module m\ncontains\nsubroutine s()\nend subroutine\nend module",
    );
    assert!(!valid.diagnostics.iter().any(|diag| {
        diag.message
            .contains("Subroutine/Function definition before CONTAINS statement")
    }));
}

#[test]
fn reports_implicit_without_enclosing_scope() {
    let parsed = ParsedFile::parse("implicit_top.f90", "implicit none");
    assert!(parsed.diagnostics.iter().any(|diag| {
        diag.message
            .contains("IMPLICIT statement without enclosing scope")
    }));

    let valid = ParsedFile::parse(
        "implicit_program.f90",
        "program app\nimplicit none\nend program",
    );
    assert!(!valid.diagnostics.iter().any(|diag| {
        diag.message
            .contains("IMPLICIT statement without enclosing scope")
    }));
}

#[test]
fn reports_import_statements_outside_interfaces() {
    let parsed = ParsedFile::parse("bad_import.f90", "module m\nimport, only: rk\nend module");
    assert!(parsed.diagnostics.iter().any(|diag| diag
        .message
        .contains("IMPORT statement outside of interface")));

    let valid = ParsedFile::parse(
        "valid_import.f90",
        "module m\ninteger :: rk\ninterface\nimport, only: rk\nend interface\nend module",
    );
    assert!(!valid.diagnostics.iter().any(|diag| diag
        .message
        .contains("IMPORT statement outside of interface")));
}

#[test]
fn reports_argument_declaration_diagnostics() {
    let parsed = ParsedFile::parse(
        "args.f90",
        "module m\n\
implicit none\n\
contains\n\
subroutine missing_decl(a, b)\n\
integer, intent(in) :: a\n\
end subroutine\n\
subroutine stray_intent(a)\n\
integer, intent(in) :: a\n\
integer, intent(out) :: not_an_arg\n\
end subroutine\n\
end module",
    );
    assert!(parsed.diagnostics.iter().any(|diag| {
        diag.message
            .contains("No matching declaration found for argument \"b\"")
    }));
    assert!(parsed.diagnostics.iter().any(|diag| {
        diag.message
            .contains("Variable \"not_an_arg\" with INTENT keyword not found in argument list")
    }));
}

#[test]
fn implicit_typing_allows_undeclared_arguments() {
    let parsed = ParsedFile::parse("implicit_args.f90", "subroutine allowed(a)\nend subroutine");
    assert!(!parsed.diagnostics.iter().any(|diag| {
        diag.message
            .contains("No matching declaration found for argument")
    }));
}

#[test]
fn handles_free_form_continuations() {
    let parsed = ParsedFile::parse(
        "cont.f90",
        "subroutine long(&\n  a, b)\ninteger :: a\nend subroutine",
    );
    let sub = parsed.symbols.iter().find(|s| s.name == "long").unwrap();
    assert_eq!(sub.args, vec!["a", "b"]);
}

#[test]
fn free_form_continuations_skip_comment_only_lines() {
    let parsed = ParsedFile::parse(
        "params.f90",
        "module m\n\
integer, parameter, public :: &\n\
! first value\n\
os_unknown = 0, &\n\
! second value\n\
os_linux = 1\n\
end module",
    );

    let names: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|sym| sym.kind == SymbolKind::Variable)
        .map(|sym| sym.name.as_str())
        .collect();
    assert!(names.contains(&"os_unknown"));
    assert!(names.contains(&"os_linux"));
}

#[test]
fn continued_procedure_headers_keep_dummy_declarations_after_doc_comments() {
    let parsed = ParsedFile::parse(
        "logger.f90",
        "module m\n\
implicit none\n\
type logger_type\n\
end type\n\
contains\n\
pure subroutine configuration(self, add_blank_line, &\n\
    time_stamp)\n\
!! version: experimental\n\
!! Reports the logging configuration of self.\n\
!! call logger % configuration( &\n\
!!     time_stamp=time_stamp )\n\
class(logger_type), intent(in) :: self\n\
logical, intent(out), optional :: add_blank_line\n\
logical, intent(out), optional :: time_stamp\n\
end subroutine\n\
end module",
    );

    assert!(parsed
        .diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("self")));
}

#[test]
fn property_checks_free_form_continuation_and_comment_mixes() {
    let mut seed = 0x5eed_f0u32;
    let mut source = String::new();
    let mut expected = Vec::new();

    for idx in 0..96 {
        let name = format!("free_{idx}");
        expected.push(name.clone());
        if next_u32(&mut seed) % 3 == 0 {
            source.push_str("! generated comment before subroutine\n");
        }
        match next_u32(&mut seed) % 4 {
            0 => source.push_str(&format!("subroutine {name}(a, b)\n")),
            1 => source.push_str(&format!("subroutine {name}(&\n  &a, b)\n")),
            2 => source.push_str(&format!("subroutine {name}(a, &\n  b)\n")),
            _ => source.push_str(&format!("subroutine {name}( &\n  &a, &\n  &b)\n")),
        }
        if next_u32(&mut seed) % 2 == 0 {
            source.push_str("! generated comment inside body\n");
        }
        source.push_str("integer :: a\n");
        source.push_str("end subroutine\n");
    }

    let parsed = ParsedFile::parse("generated.f90", &source);
    for name in expected {
        let sub = parsed
            .symbols
            .iter()
            .find(|sym| sym.name == name)
            .unwrap_or_else(|| panic!("missing generated subroutine {name}"));
        assert_eq!(sub.args, vec!["a", "b"]);
    }
}

#[test]
fn builds_hierarchical_document_symbols() {
    let parsed = ParsedFile::parse(
        "math.f90",
        "module math\ncontains\nsubroutine axpy()\ninteger :: i\nend subroutine\nend module",
    );
    let docs = parsed.document_symbols();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "math");
    assert_eq!(docs[0].children[0].name, "axpy");
    assert_eq!(docs[0].children[0].children[0].name, "i");
}

#[test]
fn document_symbols_include_nested_submodule_parent() {
    let parsed = ParsedFile::parse(
        "grandchild.f90",
        "submodule(parent:child1) grandchild\n\
contains\n\
module procedure my_fun\n\
end procedure\n\
end submodule",
    );
    let docs = parsed.document_symbols();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "child1");
    assert_eq!(docs[0].children[0].name, "grandchild");
    assert!(docs[0].children[0]
        .children
        .iter()
        .any(|symbol| symbol.name == "my_fun"));
}

#[test]
fn document_symbols_include_type_bound_generic_bindings() {
    let parsed = ParsedFile::parse(
        "types.f90",
        "module m\n\
type list\n\
contains\n\
procedure :: get_item\n\
generic, public :: get => get_item\n\
end type\n\
end module",
    );
    let docs = parsed.document_symbols();
    let ty = docs[0]
        .children
        .iter()
        .find(|symbol| symbol.name == "list")
        .unwrap();
    assert!(ty.children.iter().any(|symbol| {
        symbol.name == "get"
            && symbol
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("get_item"))
    }));
}

#[test]
fn workspace_exposes_document_symbols() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\ncontains\nsubroutine axpy()\ninteger :: i\nend subroutine\nend module",
    );

    let docs = ws.document_symbols(Path::new("math.f90"));
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "math");
    assert_eq!(docs[0].children[0].name, "axpy");
    assert!(ws.document_symbols(Path::new("missing.f90")).is_empty());
}

#[test]
fn workspace_selection_range_expands_to_enclosing_scopes() {
    let source =
        "module math\ncontains\nsubroutine axpy()\ninteger :: value\nvalue = 1\nend subroutine\nend module";
    let mut ws = Workspace::new();
    ws.upsert_file(PathBuf::from("math.f90"), source);

    let selection = ws
        .selection_range(Path::new("math.f90"), Position::new(4, 1), source)
        .expect("selection range at value reference");

    assert_eq!(selection.range.start, Position::new(4, 0));
    assert_eq!(selection.range.end, Position::new(4, 5));
    let subroutine_selection = selection.parent.as_ref().expect("subroutine name");
    assert_eq!(subroutine_selection.range.start, Position::new(2, 11));
    assert_eq!(subroutine_selection.range.end, Position::new(2, 15));
    let subroutine_range = subroutine_selection
        .parent
        .as_ref()
        .expect("subroutine range");
    assert_eq!(subroutine_range.range.start.line, 2);
    assert_eq!(subroutine_range.range.end.line, 5);
    let module_selection = subroutine_range.parent.as_ref().expect("module name");
    assert_eq!(module_selection.range.start, Position::new(0, 7));
    assert_eq!(module_selection.range.end, Position::new(0, 11));
}

#[test]
fn parses_operator_and_assignment_interface_symbols() {
    let parsed = ParsedFile::parse(
        "ops.f90",
        "module ops\n\
interface operator(+)\n\
module procedure add_vec\n\
end interface\n\
interface assignment(=)\n\
module procedure assign_vec\n\
end interface\n\
end module",
    );
    let operator = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "operator(+)")
        .unwrap();
    assert_eq!(operator.kind, SymbolKind::Interface);
    let assignment = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "assignment(=)")
        .unwrap();
    assert_eq!(assignment.kind, SymbolKind::Interface);

    let docs = parsed.document_symbols();
    assert_eq!(docs[0].children[0].name, "operator(+)");
    assert_eq!(docs[0].children[1].name, "assignment(=)");
}

#[test]
fn semantic_tokens_classify_fortran_symbols_for_freight_legend() {
    let mut ws = Workspace::new();
    let src = "#define LIMIT 4\n\
module math\n\
type :: vector\n\
  real :: x\n\
contains\n\
  procedure :: scale => scale_vector\n\
end type\n\
contains\n\
subroutine scale_vector(self, factor)\n\
  class(vector) :: self\n\
  real :: factor\n\
  integer :: max_n = LIMIT\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("math.f90"), src);

    let tokens = ws.semantic_tokens(Path::new("math.f90"));
    assert_eq!(
        token_type_at(src, &tokens, "module math", "math"),
        Some(semantic_token_type::NAMESPACE)
    );
    assert_eq!(
        token_type_at(src, &tokens, "type :: vector", "vector"),
        Some(semantic_token_type::TYPE)
    );
    assert_eq!(
        token_type_at(src, &tokens, "real :: x", "x"),
        Some(semantic_token_type::PROPERTY)
    );
    assert_eq!(
        token_type_at(src, &tokens, "procedure :: scale", "scale"),
        Some(semantic_token_type::METHOD)
    );
    assert_eq!(
        token_type_at(src, &tokens, "subroutine scale_vector", "scale_vector"),
        Some(semantic_token_type::FUNCTION)
    );
    assert_eq!(
        token_type_at(src, &tokens, "class(vector) :: self", "self"),
        Some(semantic_token_type::PARAMETER)
    );
    assert_eq!(
        token_type_at(src, &tokens, "#define LIMIT", "LIMIT"),
        Some(semantic_token_type::MACRO)
    );
    assert_eq!(
        token_type_at(src, &tokens, "integer :: max_n = LIMIT", "LIMIT"),
        Some(semantic_token_type::MACRO)
    );

    let data = ws.semantic_token_data(Path::new("math.f90"));
    assert_eq!(data.len(), tokens.len() * 5);
    assert!(!data.is_empty());
}

#[test]
fn parses_construct_scopes_and_associate_aliases() {
    let mut ws = Workspace::new();
    let src = "subroutine work(obj)\n\
block\n\
  integer :: local\n\
end block\n\
named: block\n\
end block named\n\
associate(alias => obj)\n\
  alias = alias\n\
end associate\n\
select type (obj)\n\
class is(vector)\n\
type is(point)\n\
class default\n\
end select\n\
select rank (obj)\n\
rank(1)\n\
rank default\n\
end select\n\
end subroutine";
    ws.upsert_file(PathBuf::from("work.f90"), src);
    let parsed = ws.file(Path::new("work.f90")).unwrap();

    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.kind == SymbolKind::Block));
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.kind == SymbolKind::Block && sym.name == "named"));
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.kind == SymbolKind::Associate));
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.kind == SymbolKind::SelectType));
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.kind == SymbolKind::SelectType
            && sym.signature.eq_ignore_ascii_case("select rank (obj)")));
    assert!(parsed.diagnostics.iter().all(|diag| !diag
        .message
        .contains("end statement has no matching select type scope")));
    assert!(!parsed
        .symbols
        .iter()
        .any(|sym| sym.name.eq_ignore_ascii_case("is")));
    let alias = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "alias")
        .unwrap();
    assert!(alias
        .scope
        .iter()
        .any(|part| part.starts_with("associate@")));

    let definition = ws
        .definition(Path::new("work.f90"), Position::new(7, 3), src)
        .unwrap();
    assert_eq!(definition.name, "alias");
    assert!(definition
        .scope
        .iter()
        .any(|part| part.starts_with("associate@")));
}

#[test]
fn block_scopes_shadow_outer_symbols() {
    let mut ws = Workspace::new();
    let src = "subroutine work()\n\
integer :: value\n\
block\n\
  integer :: value\n\
  value = 1\n\
end block\n\
value = 2\n\
end subroutine";
    ws.upsert_file(PathBuf::from("work.f90"), src);

    let inner = ws
        .definition(Path::new("work.f90"), Position::new(4, 3), src)
        .unwrap();
    assert_eq!(inner.range.start.line, 3);
    assert!(inner.scope.iter().any(|part| part.starts_with("block@")));

    let outer = ws
        .definition(Path::new("work.f90"), Position::new(6, 1), src)
        .unwrap();
    assert_eq!(outer.range.start.line, 1);
    assert_eq!(outer.scope, vec!["work"]);
}

#[test]
fn attaches_doc_comments_to_next_scope() {
    let parsed = ParsedFile::parse(
            "math.f90",
            "module math\n!! Scale and add vectors.\npure subroutine axpy(a, x, y)\nend subroutine\nend module",
        );
    let axpy = parsed.symbols.iter().find(|s| s.name == "axpy").unwrap();
    assert_eq!(
        axpy.documentation.as_deref(),
        Some("Scale and add vectors.")
    );
    assert_eq!(axpy.kind, SymbolKind::Subroutine);
}

#[test]
fn signature_help_tracks_active_parameter() {
    let mut ws = Workspace::new();
    let src = "module math\ncontains\nsubroutine axpy(a, x, y)\nend subroutine\nend module";
    let app = "program app\nuse math, only: axpy\ncall axpy(alpha, xs, ys)\nend program";
    ws.upsert_file(PathBuf::from("math.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(2, 18), app)
        .unwrap();
    assert_eq!(sig.parameters, vec!["a", "x", "y"]);
    assert_eq!(sig.active_parameter, 1);
}

#[test]
fn signature_help_tracks_keyword_arguments() {
    let mut ws = Workspace::new();
    let src = "module math\ncontains\nsubroutine axpy(a, x, y)\nend subroutine\nend module";
    let app = "program app\nuse math, only: axpy\ncall axpy(y=ys)\nend program";
    ws.upsert_file(PathBuf::from("math.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(2, 14), app)
        .unwrap();
    assert_eq!(sig.parameters, vec!["a", "x", "y"]);
    assert_eq!(sig.active_parameter, 2);
}

#[test]
fn inlay_hints_show_positional_argument_names() {
    let mut ws = Workspace::new();
    let src = "module math\ncontains\nsubroutine axpy(a, x, y)\nend subroutine\nend module";
    let app = "program app\nuse math, only: axpy\ncall axpy(alpha, xs, y=ys)\nend program";
    ws.upsert_file(PathBuf::from("math.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let hints = ws.inlay_hints(Path::new("app.f90"), 2, 2);
    let labels: Vec<_> = hints.iter().map(|hint| hint.label.as_str()).collect();
    assert_eq!(labels, vec!["a:", "x:"]);
    assert_eq!(
        hints[0].position.character,
        "call axpy(".encode_utf16().count()
    );
    assert_eq!(
        hints[1].position.character,
        "call axpy(alpha, ".encode_utf16().count()
    );
}

#[test]
fn diagnostics_report_bad_procedure_call_arguments() {
    let mut ws = Workspace::new();
    let src = "module math\ncontains\nsubroutine axpy(a, x)\nend subroutine\nend module";
    let app = "program app\n\
use math, only: axpy\n\
call axpy(alpha, xs, ys)\n\
call axpy(scale=alpha)\n\
call axpy(a=alpha, a=xs)\n\
end program";
    ws.upsert_file(PathBuf::from("math.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 3);
    assert!(diagnostics
        .iter()
        .any(|diag| diag.message.contains("too many positional arguments")));
    assert!(diagnostics
        .iter()
        .any(|diag| diag.message.contains("no argument named `scale`")));
    assert!(diagnostics
        .iter()
        .any(|diag| diag.message.contains("repeats argument `a`")));
}

#[test]
fn call_argument_parser_keeps_array_constructors_together() {
    let mut ws = Workspace::new();
    let src = "module math\n\
contains\n\
subroutine set(key, value)\n\
integer :: key\n\
integer :: value(:)\n\
end subroutine\n\
end module";
    let app = "program app\n\
use math, only: set\n\
call set(key, [0, 1])\n\
end program";
    ws.upsert_file(PathBuf::from("math.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("too many positional arguments")),
        "{diagnostics:?}"
    );
}

#[test]
fn local_array_references_do_not_fall_back_to_intrinsic_call_diagnostics() {
    let mut ws = Workspace::new();
    let app = "program app\n\
implicit none\n\
real :: loc(2, 2, 2)\n\
print *, loc(:, :, 1)\n\
end program";
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `loc`")),
        "{diagnostics:?}"
    );
}

#[test]
fn unresolved_use_suppresses_intrinsic_call_argument_cascades() {
    let mut ws = Workspace::new();
    let app = "program app\n\
use strings\n\
implicit none\n\
print *, char(value, 1, 4)\n\
end program";
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(diagnostics
        .iter()
        .any(|diag| diag.message.contains("module `strings`")));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `char`")),
        "{diagnostics:?}"
    );
}

#[test]
fn unresolved_only_reexports_suppress_generic_call_argument_cascades() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\n\
use external_toml, only: get_value\n\
implicit none\n\
interface get_value\n\
module procedure get_value_logical\n\
end interface\n\
contains\n\
subroutine get_value_logical(table, key, value, error)\n\
integer :: table\n\
character(len=*) :: key\n\
logical :: value\n\
integer :: error\n\
end subroutine\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use wrapper, only: get_value\n\
implicit none\n\
integer :: table, stat\n\
character(len=8) :: name\n\
call get_value(table, 'name', name, stat=stat)\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `get_value`")),
        "{diagnostics:?}"
    );
}

#[test]
fn generic_overload_selection_requires_all_non_optional_arguments() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("toml_type.f90"),
        "module toml_type\n\
implicit none\n\
interface add_table\n\
module procedure add_table_to_table\n\
module procedure add_table_to_array\n\
end interface\n\
contains\n\
subroutine add_table_to_table(table, key, ptr, stat)\n\
integer, intent(inout) :: table\n\
character(len=*), intent(in) :: key\n\
integer, intent(out) :: ptr\n\
integer, intent(out), optional :: stat\n\
end subroutine\n\
subroutine add_table_to_array(array, ptr, stat)\n\
integer, intent(inout) :: array\n\
integer, intent(out) :: ptr\n\
integer, intent(out), optional :: stat\n\
end subroutine\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use toml_type, only: add_table\n\
implicit none\n\
integer :: array, ptr\n\
call add_table(array, ptr)\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("missing required argument `ptr`")),
        "{diagnostics:?}"
    );
}

#[test]
fn diagnostics_report_missing_required_procedure_arguments() {
    let mut ws = Workspace::new();
    let src = "module log_mod\n\
contains\n\
subroutine write_log(message, unit)\n\
character(len=*) :: message\n\
integer, optional :: unit\n\
end subroutine\n\
end module";
    let app = "program app\n\
use log_mod, only: write_log\n\
call write_log()\n\
call write_log('ok')\n\
end program";
    ws.upsert_file(PathBuf::from("log.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("missing required argument `message`"));
}

#[test]
fn generic_interface_calls_use_linked_module_procedure_signatures() {
    let mut ws = Workspace::new();
    let src = "module m\n\
interface set\n\
module procedure set_one\n\
module procedure set_pair\n\
end interface\n\
contains\n\
subroutine set_one(x)\n\
integer :: x\n\
end subroutine\n\
subroutine set_pair(x, y)\n\
integer :: x\n\
integer :: y\n\
end subroutine\n\
end module\n\
program app\n\
use m\n\
call set(1, 2)\n\
call set(1, extra=2)\n\
end program";
    ws.upsert_file(PathBuf::from("app.f90"), src);

    let signature = ws
        .signature_help(Path::new("app.f90"), Position::new(16, 12), src)
        .unwrap();
    assert_eq!(signature.label, "subroutine set_pair(x, y)");
    assert_eq!(signature.parameters, vec!["x", "y"]);
    assert_eq!(signature.active_parameter, 1);

    let hints = ws.inlay_hints(Path::new("app.f90"), 16, 16);
    let labels: Vec<_> = hints.iter().map(|hint| hint.label.as_str()).collect();
    assert_eq!(labels, vec!["x:", "y:"]);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("call to `set` has no argument named `extra`"));
}

#[test]
fn reports_generic_interface_links_to_missing_module_procedures() {
    let mut ws = Workspace::new();
    let src = "module m\n\
interface set\n\
module procedure set_missing\n\
module procedure set_present\n\
end interface\n\
contains\n\
subroutine set_present(x)\n\
integer :: x\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let diagnostics = ws.diagnostics(Path::new("m.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("generic interface `set` references unknown module procedure `set_missing`"));
}

#[test]
fn call_diagnostics_resolve_internal_procedure_before_type_bound_method() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type core\n\
contains\n\
procedure :: traverse => core_traverse\n\
end type\n\
contains\n\
subroutine core_traverse(self, node, callback)\n\
class(core), intent(inout) :: self\n\
integer, intent(in) :: node\n\
integer, intent(in) :: callback\n\
end subroutine\n\
subroutine outer(json, node)\n\
class(core), intent(inout) :: json\n\
integer, intent(in) :: node\n\
call traverse(node)\n\
contains\n\
subroutine traverse(node)\n\
integer, intent(in) :: node\n\
end subroutine\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("shadow.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("shadow.f90"));
    assert!(
        diagnostics.iter().all(|diag| !diag
            .message
            .contains("missing required argument `callback`")),
        "{diagnostics:?}"
    );
}

#[test]
fn cursor_queries_use_utf16_columns() {
    let mut ws = Workspace::new();
    let src = "module math\ncontains\nsubroutine axpy(a, x, y)\nend subroutine\nend module";
    let app =
        "program app\nuse math, only: axpy\nprint *, \"🙂\"; call axpy(alpha, xs, ys)\nend program";
    ws.upsert_file(PathBuf::from("math.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let hover_col = "print *, \"🙂\"; call ax".encode_utf16().count();
    let hover = ws
        .hover(Path::new("app.f90"), Position::new(2, hover_col), app)
        .unwrap();
    assert!(hover.contains("subroutine axpy"));

    let sig_col = "print *, \"🙂\"; call axpy(alpha, x".encode_utf16().count();
    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(2, sig_col), app)
        .unwrap();
    assert_eq!(sig.active_parameter, 1);

    let refs = ws.references(Path::new("app.f90"), Position::new(2, hover_col), app);
    let call_ref = refs
        .iter()
        .find(|loc| loc.file == Path::new("app.f90") && loc.range.start.line == 2)
        .unwrap();
    let expected_start = "print *, \"🙂\"; call ".encode_utf16().count();
    assert_eq!(call_ref.range.start.character, expected_start);
}

#[test]
fn diagnostics_do_not_panic_on_non_ascii_string_lines() {
    let mut ws = Workspace::new();
    let src = "program app\n\
print '(a)', '✓ DEBUG mode enabled'\n\
end program";
    ws.upsert_file(PathBuf::from("unicode.f90"), src);
    assert!(ws.diagnostics(Path::new("unicode.f90")).is_empty());
}

#[test]
fn reports_unresolved_use_modules_and_only_names() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\ncontains\nsubroutine axpy()\nend subroutine\nend module",
    );
    ws.upsert_file(
            PathBuf::from("app.f90"),
            "program app\nuse missing\nuse math, only: nope\nuse, intrinsic :: iso_fortran_env\nend program",
        );
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 2);
    assert!(diagnostics[0].message.contains("could not be resolved"));
    assert!(diagnostics[1].message.contains("does not export"));
}

#[test]
fn unresolved_module_dependencies_suppress_precise_export_diagnostics() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\nuse missing_dep\nimplicit none\nend module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse wrapper, only: maybe_from_missing_dep\nend program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("does not export")));
}

#[test]
fn use_only_accepts_public_enumerators_and_continued_parameters() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("compiler.F90"),
        "module compiler\n\
implicit none\n\
enum, bind(C)\n\
enumerator :: &\n\
id_unknown, &\n\
id_gcc, &\n\
id_cray\n\
end enum\n\
character(*), parameter :: &\n\
flag_gnu_openmp = ' -fopenmp', &\n\
flag_gnu_fixed_form = ' -ffixed-form'\n\
character(*), parameter :: &\n\
flag_gnu_warn = ' -Wall', & ! inline comments after continuation markers stay continued\n\
flag_gnu_openmp_commented = ' -fopenmp'\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use compiler, only: id_cray, flag_gnu_openmp, flag_gnu_openmp_commented\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("does not export")),
        "{diagnostics:?}"
    );
}

#[test]
fn unresolved_only_reexports_report_module_unresolved_for_use_only() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\n\
use external_dep, only: external_type\n\
implicit none\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use wrapper, only: external_type\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("module `wrapper`")));
    assert!(diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("does not export")));
}

#[test]
fn whole_module_use_accepts_partially_indexed_module() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\nuse missing_dep\nimplicit none\nend module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse wrapper\nend program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn whole_module_use_reports_partially_indexed_module_with_local_api() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\n\
use missing_dep\n\
implicit none\n\
type :: key_type\n\
end type\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "module app\nuse wrapper\nend module",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "module `wrapper` could not be resolved"
    );
}

#[test]
fn program_use_accepts_partially_indexed_module_with_local_api() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\n\
use missing_dep\n\
implicit none\n\
type :: key_type\n\
end type\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse wrapper\nend program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn unresolved_private_only_dependency_does_not_poison_whole_module_use() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("wrapper.f90"),
        "module wrapper\n\
use missing_dep, only: helper\n\
implicit none\n\
private\n\
public :: value\n\
integer :: value\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse wrapper\nend program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("wrapper")),
        "{diagnostics:?}"
    );
}

#[test]
fn finds_references_across_used_modules() {
    let mut ws = Workspace::new();
    let math = "module math\ncontains\nsubroutine axpy()\nend subroutine axpy\nend module";
    let app = "program app\nuse math, only: axpy\ncall axpy()\nend program";
    ws.upsert_file(PathBuf::from("math.f90"), math);
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let refs = ws.references(Path::new("app.f90"), Position::new(2, 6), app);
    assert!(refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("math.f90") && loc.range.start.line == 2));
    assert!(refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("app.f90") && loc.range.start.line == 2));
}

#[test]
fn rename_returns_workspace_text_edits() {
    let mut ws = Workspace::new();
    let math = "module math\ncontains\nsubroutine axpy()\nend subroutine axpy\nend module";
    let app = "program app\nuse math, only: axpy\ncall axpy()\nend program";
    ws.upsert_file(PathBuf::from("math.f90"), math);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let edits = ws
        .rename(Path::new("app.f90"), Position::new(2, 6), app, "saxpy")
        .unwrap();
    assert!(edits
        .iter()
        .any(|edit| edit.file == PathBuf::from("math.f90")
            && edit.range.start.line == 2
            && edit.new_text == "saxpy"));
    assert!(edits
        .iter()
        .any(|edit| edit.file == PathBuf::from("app.f90")
            && edit.range.start.line == 2
            && edit.new_text == "saxpy"));
}

#[test]
fn rename_rejects_invalid_identifiers_and_scope_conflicts() {
    let mut ws = Workspace::new();
    let src = "module math\ncontains\nsubroutine axpy()\nend subroutine\nsubroutine saxpy()\nend subroutine\nend module";
    ws.upsert_file(PathBuf::from("math.f90"), src);

    assert_eq!(
        ws.rename(Path::new("math.f90"), Position::new(2, 12), src, "1bad"),
        Err(RenameError::InvalidIdentifier)
    );
    assert!(matches!(
        ws.rename(Path::new("math.f90"), Position::new(2, 12), src, "saxpy"),
        Err(RenameError::ConflictingSymbol { .. })
    ));
}

#[test]
fn respects_module_private_default_and_public_lists() {
    let mut ws = Workspace::new();
    ws.upsert_file(
            PathBuf::from("math.f90"),
            "module math\nprivate\npublic :: axpy\ncontains\nsubroutine axpy()\nend subroutine\nsubroutine hidden()\nend subroutine\nend module",
        );
    let app = "program app\nuse math, only: axpy, hidden\ncall axpy()\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("hidden"));
    assert!(ws
        .definition(Path::new("app.f90"), Position::new(2, 6), app)
        .is_some_and(|sym| sym.name == "axpy"));
}

#[test]
fn completions_in_visibility_statements_offer_local_visible_object_kinds_only() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("other.f90"),
        "module other\ninteger :: alien\nend module",
    );
    let src = "module m\n\
use other, only: alien\n\
private\n\
public :: a\n\
public :: s\n\
integer :: apple\n\
type :: axis\n\
end type\n\
interface solve\n\
module procedure solve_impl\n\
end interface\n\
contains\n\
subroutine axpy()\n\
end subroutine\n\
function area() result(value)\n\
integer :: value\n\
end function\n\
subroutine share()\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let a_items = ws.completions_at(Path::new("m.f90"), Position::new(3, 99), "a");
    assert!(a_items.iter().any(|item| item.label == "apple"));
    assert!(a_items.iter().any(|item| item.label == "axis"));
    assert!(a_items.iter().any(|item| item.label == "axpy"));
    assert!(a_items.iter().any(|item| item.label == "area"));
    assert!(!a_items.iter().any(|item| item.label == "alien"));

    let s_items = ws.completions_at(Path::new("m.f90"), Position::new(4, 99), "s");
    assert!(s_items.iter().any(|item| item.label == "share"));
    assert!(!s_items.iter().any(|item| item.label == "solve"));
    assert!(!s_items.iter().any(|item| item.label == "solve_impl"));
}

#[test]
fn completions_follow_local_scope() {
    let mut ws = Workspace::new();
    let src = "module m\n\
contains\n\
subroutine work()\n\
integer :: work_value\n\
wo\n\
end subroutine\n\
subroutine other()\n\
integer :: other_value\n\
ot\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let work_items = ws.completions_at(Path::new("m.f90"), Position::new(4, 2), "w");
    assert!(work_items.iter().any(|item| item.label == "work_value"));
    assert!(!work_items.iter().any(|item| item.label == "other_value"));

    let other_items = ws.completions_at(Path::new("m.f90"), Position::new(8, 2), "o");
    assert!(other_items.iter().any(|item| item.label == "other_value"));
    assert!(!other_items.iter().any(|item| item.label == "work_value"));
}

#[test]
fn completions_respect_module_export_visibility() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\nprivate\npublic :: axpy\ncontains\nsubroutine axpy()\nend subroutine\nsubroutine hidden()\nend subroutine\nend module",
    );
    let app = "program app\nuse math\ncall a\ncall h\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let public_items = ws.completions_at(Path::new("app.f90"), Position::new(2, 6), "a");
    assert!(public_items.iter().any(|item| item.label == "axpy"));

    let hidden_items = ws.completions_at(Path::new("app.f90"), Position::new(3, 6), "h");
    assert!(!hidden_items.iter().any(|item| item.label == "hidden"));
}

#[test]
fn completions_respect_use_only_lists() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\ncontains\nsubroutine axpy()\nend subroutine\nsubroutine hidden()\nend subroutine\nend module",
    );
    let app = "program app\nuse math, only: axpy\ncall a\ncall h\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let public_items = ws.completions_at(Path::new("app.f90"), Position::new(2, 6), "a");
    assert!(public_items.iter().any(|item| item.label == "axpy"));

    let hidden_items = ws.completions_at(Path::new("app.f90"), Position::new(3, 6), "h");
    assert!(!hidden_items.iter().any(|item| item.label == "hidden"));
}

#[test]
fn completions_offer_modules_and_only_members_in_use_statements() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("math.f90"),
        "module math\ninteger, public :: rk\ninteger, private :: hidden\ncontains\nsubroutine axpy()\nend subroutine\nend module",
    );
    let app =
        "program app\nuse ma\nuse math, only: ax\nuse iso_fortran_env, only: int\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let modules = ws.completions_at(Path::new("app.f90"), Position::new(1, 6), "ma");
    assert!(modules.iter().any(|item| item.label == "math"));
    assert!(!modules.iter().any(|item| item.label == "axpy"));

    let members = ws.completions_at(Path::new("app.f90"), Position::new(2, 19), "ax");
    assert!(members.iter().any(|item| item.label == "axpy"));
    assert!(!members.iter().any(|item| item.label == "hidden"));
    assert!(!members.iter().any(|item| item.label == "math"));

    let intrinsic_members = ws.completions_at(Path::new("app.f90"), Position::new(3, 32), "int");
    assert!(intrinsic_members
        .iter()
        .any(|item| item.label.eq_ignore_ascii_case("int32")));
}

#[test]
fn completions_for_module_procedure_links_offer_local_procedures_only() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("other.f90"),
        "module other\ncontains\nsubroutine set_imported()\nend subroutine\nend module",
    );
    let src = "module m\n\
use other, only: set_imported\n\
integer :: set_count\n\
interface set\n\
module procedure set_\n\
end interface\n\
contains\n\
subroutine set_one()\n\
end subroutine\n\
function set_value() result(value)\n\
integer :: value\n\
end function\n\
subroutine helper()\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let items = ws.completions_at(Path::new("m.f90"), Position::new(4, 21), "set_");
    assert!(items.iter().any(|item| item.label == "set_one"));
    assert!(items.iter().any(|item| item.label == "set_value"));
    assert!(!items.iter().any(|item| item.label == "set_imported"));
    assert!(!items.iter().any(|item| item.label == "set_count"));
    assert!(!items
        .iter()
        .any(|item| item.detail == "module procedure set_"));
}

#[test]
fn completions_in_import_statements_offer_host_variables_and_types_only() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("other.f90"),
        "module other\ninteger :: remote_value\nend module",
    );
    let src = "module m\n\
use other, only: remote_value\n\
integer :: rk\n\
type :: vector\n\
end type\n\
interface\n\
import, only: r\n\
import, only: v\n\
import, only: h\n\
subroutine s(x)\n\
integer :: x\n\
end subroutine\n\
end interface\n\
contains\n\
subroutine helper()\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let variable_items = ws.completions_at(Path::new("m.f90"), Position::new(6, 99), "r");
    assert!(variable_items.iter().any(|item| item.label == "rk"));
    assert!(!variable_items
        .iter()
        .any(|item| item.label == "remote_value"));

    let type_items = ws.completions_at(Path::new("m.f90"), Position::new(7, 99), "v");
    assert!(type_items.iter().any(|item| item.label == "vector"));

    let procedure_items = ws.completions_at(Path::new("m.f90"), Position::new(8, 99), "h");
    assert!(!procedure_items.iter().any(|item| item.label == "helper"));
}

#[test]
fn completions_offer_declaration_keywords_before_double_colon() {
    let mut ws = Workspace::new();
    let src = "module m\n\
integer, par\n\
integer, opt\n\
type :: shape\n\
contains\n\
procedure, de\n\
end type\n\
contains\n\
subroutine run(x)\n\
integer, opt\n\
integer :: old_value\n\
integer :: o\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let module_items = ws.completions_at(Path::new("m.f90"), Position::new(1, 12), "par");
    assert!(module_items.iter().any(|item| item.label == "parameter"));
    assert!(!module_items.iter().any(|item| item.label == "optional"));

    let module_optional_items = ws.completions_at(Path::new("m.f90"), Position::new(2, 12), "opt");
    assert!(!module_optional_items
        .iter()
        .any(|item| item.label == "optional"));

    let type_items = ws.completions_at(Path::new("m.f90"), Position::new(5, 13), "de");
    assert!(type_items.iter().any(|item| item.label == "deferred"));
    assert!(!type_items.iter().any(|item| item.label == "optional"));

    let procedure_items = ws.completions_at(Path::new("m.f90"), Position::new(9, 12), "opt");
    assert!(procedure_items.iter().any(|item| item.label == "optional"));
    assert!(!procedure_items.iter().any(|item| item.label == "deferred"));

    let symbol_items = ws.completions_at(Path::new("m.f90"), Position::new(11, 12), "o");
    assert!(symbol_items.iter().any(|item| item.label == "old_value"));
    assert!(!symbol_items.iter().any(|item| item.label == "optional"));
}

#[test]
fn completions_in_declaration_variable_lists_offer_variables_only() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("other.f90"),
        "module other\ninteger :: old_remote\ncontains\nsubroutine other_call()\nend subroutine\nend module",
    );
    let src = "module m\n\
use other, only: old_remote, other_call\n\
integer :: old_local\n\
type :: old_type\n\
end type\n\
contains\n\
subroutine run()\n\
integer :: old_inner\n\
integer :: old_\n\
end subroutine\n\
subroutine old_call()\n\
end subroutine\n\
function old_fun() result(value)\n\
integer :: value\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let items = ws.completions_at(Path::new("m.f90"), Position::new(8, 99), "old_");
    assert!(items.iter().any(|item| item.label == "old_local"));
    assert!(items.iter().any(|item| item.label == "old_inner"));
    assert!(items.iter().any(|item| item.label == "old_remote"));
    assert!(!items.iter().any(|item| item.label == "old_type"));
    assert!(!items.iter().any(|item| item.label == "old_call"));
    assert!(!items.iter().any(|item| item.label == "old_fun"));
    assert!(!items.iter().any(|item| item.label == "other_call"));
}

#[test]
fn completions_at_first_word_offer_fortran_statements_plus_visible_symbols() {
    let mut ws = Workspace::new();
    let src = "module m\n\
contains\n\
subroutine run()\n\
integer :: case_count\n\
ca\n\
call ca\n\
end subroutine\n\
subroutine case_call()\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let first_word_items = ws.completions_at(Path::new("m.f90"), Position::new(4, 2), "ca");
    assert!(first_word_items.iter().any(|item| item.label == "call"));
    assert!(first_word_items
        .iter()
        .any(|item| item.label == "case_count"));
    assert!(first_word_items
        .iter()
        .any(|item| item.label == "case_call"));

    let call_items = ws.completions_at(Path::new("m.f90"), Position::new(5, 7), "ca");
    assert!(call_items.iter().any(|item| item.label == "case_call"));
    assert!(!call_items.iter().any(|item| item.label == "call"));
    assert!(!call_items.iter().any(|item| item.label == "case_count"));
}

#[test]
fn completions_skip_scope_declarations_and_end_statements() {
    let mut ws = Workspace::new();
    let src = "module m\n\
contains\n\
subroutine run()\n\
integer :: case_count\n\
ca\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let module_items = ws.completions_at(Path::new("m.f90"), Position::new(0, 8), "m");
    assert!(module_items.is_empty());

    let subroutine_items = ws.completions_at(Path::new("m.f90"), Position::new(2, 12), "ru");
    assert!(subroutine_items.is_empty());

    let end_items = ws.completions_at(Path::new("m.f90"), Position::new(5, 3), "end");
    assert!(end_items.is_empty());

    let first_word_items = ws.completions_at(Path::new("m.f90"), Position::new(4, 2), "ca");
    assert!(first_word_items.iter().any(|item| item.label == "call"));
}

#[test]
fn completions_offer_visible_types_in_type_class_and_extends_contexts() {
    let mut ws = Workspace::new();
    let types = "module shapes\n\
type :: vector\n\
end type\n\
type, private :: hidden_shape\n\
end type\n\
end module";
    let app = "program app\n\
use shapes\n\
type(ve\n\
class(ve\n\
type, extends(ve) :: child\n\
end type\n\
integer :: value\n\
end program";
    ws.upsert_file(PathBuf::from("shapes.f90"), types);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let type_items = ws.completions_at(Path::new("app.f90"), Position::new(2, 7), "ve");
    assert!(type_items.iter().any(|item| item.label == "vector"));
    assert!(!type_items.iter().any(|item| item.label == "hidden_shape"));
    assert!(!type_items.iter().any(|item| item.label == "value"));

    let class_items = ws.completions_at(Path::new("app.f90"), Position::new(3, 8), "ve");
    assert!(class_items.iter().any(|item| item.label == "vector"));

    let extends_items = ws.completions_at(Path::new("app.f90"), Position::new(4, 16), "ve");
    assert!(extends_items.iter().any(|item| item.label == "vector"));
}

#[test]
fn completions_in_procedure_interface_context_offer_abstract_interface_prototypes() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
subroutine draw_iface(self)\n\
integer :: self\n\
end subroutine\n\
end interface\n\
integer :: draw_count\n\
type :: shape\n\
contains\n\
procedure(draw_\n\
end type\n\
contains\n\
subroutine draw_impl(self)\n\
integer :: self\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    let items = ws.completions_at(Path::new("m.f90"), Position::new(9, 15), "draw_");
    assert!(items.iter().any(|item| item.label == "draw_iface"));
    assert!(!items.iter().any(|item| item.label == "draw_count"));
    assert!(!items.iter().any(|item| item.label == "draw_impl"));
}

#[test]
fn procedure_interface_completions_follow_use_only_abstract_interface_prototypes() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("interfaces.f90"),
        "module interfaces\n\
abstract interface\n\
subroutine draw_iface(self)\n\
integer :: self\n\
end subroutine\n\
end interface\n\
end module",
    );
    let src = "module m\n\
use interfaces, only: render_iface => draw_iface\n\
type :: shape\n\
contains\n\
procedure(render_\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    assert!(ws.diagnostics(Path::new("m.f90")).is_empty());
    let items = ws.completions_at(Path::new("m.f90"), Position::new(4, 17), "render_");
    assert!(items.iter().any(|item| item.label == "render_iface"));
    assert!(!items.iter().any(|item| item.label == "draw_iface"));
}

#[test]
fn completions_after_call_offer_callable_symbols_only() {
    let mut ws = Workspace::new();
    let math = "module math\n\
integer :: alpha\n\
type :: atlas\n\
end type\n\
interface solve\n\
module procedure solve_impl\n\
end interface\n\
contains\n\
subroutine axpy()\n\
end subroutine\n\
function norm() result(value)\n\
real :: value\n\
end function\n\
subroutine solve_impl()\n\
end subroutine\n\
end module";
    let hidden = "module hidden_mod\n\
private\n\
public :: visible_sub\n\
contains\n\
subroutine hidden_sub()\n\
end subroutine\n\
subroutine visible_sub()\n\
end subroutine\n\
end module";
    let app = "program app\n\
use math\n\
use hidden_mod\n\
use math, only: renamed_axpy => axpy\n\
integer :: apple\n\
call a\n\
call n\n\
call so\n\
call ren\n\
call cpu\n\
call h\n\
call axpy(ap\n\
end program";
    ws.upsert_file(PathBuf::from("math.f90"), math);
    ws.upsert_file(PathBuf::from("hidden.f90"), hidden);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let call_a = ws.completions_at(Path::new("app.f90"), Position::new(5, 6), "a");
    assert!(call_a.iter().any(|item| item.label == "axpy"));
    assert!(!call_a.iter().any(|item| item.label == "alpha"));
    assert!(!call_a.iter().any(|item| item.label == "atlas"));
    assert!(!call_a.iter().any(|item| item.label == "apple"));

    let call_n = ws.completions_at(Path::new("app.f90"), Position::new(6, 6), "n");
    assert!(!call_n.iter().any(|item| item.label == "norm"));

    let call_solve = ws.completions_at(Path::new("app.f90"), Position::new(7, 7), "so");
    assert!(call_solve.iter().any(|item| item.label == "solve"));

    let renamed = ws.completions_at(Path::new("app.f90"), Position::new(8, 8), "ren");
    assert!(renamed.iter().any(|item| item.label == "renamed_axpy"));

    let intrinsic = ws.completions_at(Path::new("app.f90"), Position::new(9, 8), "cpu");
    assert!(intrinsic
        .iter()
        .any(|item| item.label.eq_ignore_ascii_case("cpu_time")));

    let hidden_items = ws.completions_at(Path::new("app.f90"), Position::new(10, 6), "h");
    assert!(!hidden_items.iter().any(|item| item.label == "hidden_sub"));

    let argument_items = ws.completions_at(Path::new("app.f90"), Position::new(11, 12), "ap");
    assert!(argument_items.iter().any(|item| item.label == "apple"));
    assert!(!argument_items.iter().any(|item| item.label == "axpy"));
}

#[test]
fn records_declaration_attributes_and_result_names() {
    let parsed = ParsedFile::parse(
            "decl.f90",
            "module m\ninteger, parameter, public :: rk = 8\ntype(vector), pointer, private :: current\ncontains\nfunction norm(x) result(value)\nreal :: value\nend function\nend module",
        );
    let rk = parsed.symbols.iter().find(|s| s.name == "rk").unwrap();
    assert_eq!(rk.visibility, Visibility::Public);
    assert_eq!(rk.type_spec.as_deref(), Some("integer"));
    assert!(rk.is_parameter);
    let current = parsed.symbols.iter().find(|s| s.name == "current").unwrap();
    assert_eq!(current.visibility, Visibility::Private);
    assert_eq!(current.type_spec.as_deref(), Some("type(vector)"));
    assert!(current.attributes.iter().any(|attr| attr == "pointer"));
    let norm = parsed.symbols.iter().find(|s| s.name == "norm").unwrap();
    assert_eq!(norm.result.as_deref(), Some("value"));
}

#[test]
fn handles_fixed_form_continuations() {
    let parsed = ParsedFile::parse(
        "legacy.f",
        "      subroutine saxpy(a,\n     & x, y)\n      real :: a\n      end\n",
    );
    let sub = parsed.symbols.iter().find(|s| s.name == "saxpy").unwrap();
    assert_eq!(sub.args, vec!["a", "x", "y"]);
    assert_eq!(sub.range.start.line, 0);
}

#[test]
fn free_form_path_hint_overrides_fixed_extension() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("example_packages/free-form/src/lib.f"),
        "module lib\n\
contains\n\
subroutine hello\n\
print '(a)', \"Hello, free world!\"\n\
end subroutine\n\
end module",
    );

    let symbols = ws.document_symbols(Path::new("example_packages/free-form/src/lib.f"));
    let names: Vec<_> = symbols.iter().map(|sym| sym.name.as_str()).collect();
    assert!(names.contains(&"lib"), "{symbols:?}");
    assert!(
        symbols
            .iter()
            .flat_map(|sym| sym.children.iter())
            .any(|sym| sym.name == "hello"),
        "{symbols:?}"
    );
}

#[test]
fn property_checks_fixed_form_continuation_and_comment_mixes() {
    let mut seed = 0x51a7_f077u32;
    let mut source = String::new();
    let mut expected = Vec::new();

    for idx in 0..96 {
        let name = format!("fx{idx}");
        expected.push(name.clone());
        if next_u32(&mut seed) % 3 == 0 {
            source.push_str("C generated fixed-form comment\n");
        }
        match next_u32(&mut seed) % 3 {
            0 => source.push_str(&format!("      subroutine {name}(a, b)\n")),
            1 => source.push_str(&format!("      subroutine {name}(a,\n     & b)\n")),
            _ => source.push_str(&format!("      subroutine {name}(\n     & a, b)\n")),
        }
        if next_u32(&mut seed) % 2 == 0 {
            source.push_str("* generated fixed-form body comment\n");
        }
        source.push_str("      integer :: a\n");
        source.push_str("      end\n");
    }

    let parsed = ParsedFile::parse("generated.f", &source);
    for name in expected {
        let sub = parsed
            .symbols
            .iter()
            .find(|sym| sym.name == name)
            .unwrap_or_else(|| panic!("missing generated subroutine {name}"));
        assert_eq!(sub.args, vec!["a", "b"]);
    }
}

#[test]
fn records_include_statements() {
    let parsed = ParsedFile::parse(
        "inc.f90",
        "module m\ninclude 'params.inc'\n#include \"defs.inc\"\nend module",
    );
    let paths: Vec<_> = parsed
        .includes
        .iter()
        .map(|include| include.path.as_str())
        .collect();
    assert_eq!(paths, vec!["params.inc", "defs.inc"]);
    assert_eq!(parsed.includes[0].scope, vec!["m"]);
}

#[test]
fn records_interface_import_statements() {
    let parsed = ParsedFile::parse(
        "iface.f90",
        "module m\ninteger :: rk\ninterface\nimport, only: rk\nsubroutine s(x)\nreal :: x\nend subroutine\nend interface\nend module",
    );
    assert_eq!(parsed.imports.len(), 1);
    assert_eq!(parsed.imports[0].kind, crate::ImportKind::Only);
    assert_eq!(parsed.imports[0].names, vec!["rk"]);
    assert_eq!(parsed.imports[0].scope, vec!["m", "interface"]);
}

#[test]
fn validates_interface_import_only_names_against_host_scope() {
    let mut ws = Workspace::new();
    let src = "module m\n\
integer :: rk\n\
interface\n\
import, only: rk, missing\n\
subroutine s(x)\n\
real :: x\n\
end subroutine\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("iface.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("iface.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("missing"));
}

#[test]
fn import_only_accepts_host_associated_intrinsic_use_names() {
    let mut ws = Workspace::new();
    let src = "program main\n\
use iso_fortran_env, only: real64\n\
interface\n\
function clamp(v) result(r)\n\
import, only: real64\n\
real(real64), intent(in) :: v\n\
real(real64) :: r\n\
end function\n\
end interface\n\
end program";
    ws.upsert_file(PathBuf::from("iface.f90"), src);
    assert!(ws.diagnostics(Path::new("iface.f90")).is_empty());
}

#[test]
fn import_only_accepts_host_associated_user_module_reexports() {
    let mut ws = Workspace::new();
    let module = "module constants\n\
use iso_fortran_env, only: wp => real64\n\
end module";
    let app = "module fit\n\
use constants, only: wp\n\
abstract interface\n\
function expr(x) result(y)\n\
import :: wp\n\
real(wp), intent(in) :: x\n\
real(wp) :: y\n\
end function\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("constants.f90"), module);
    ws.upsert_file(PathBuf::from("fit.f90"), app);
    assert!(ws.diagnostics(Path::new("fit.f90")).is_empty());
}

#[test]
fn import_only_accepts_submodule_ancestor_host_names() {
    let mut ws = Workspace::new();
    let module = "module system\n\
integer, parameter :: process_ID = selected_int_kind(18)\n\
interface\n\
module subroutine launch()\n\
end subroutine\n\
end interface\n\
end module";
    let submodule = "submodule (system) system_subprocess\n\
interface\n\
subroutine process_create(pid)\n\
import process_ID\n\
integer(process_ID), intent(out) :: pid\n\
end subroutine\n\
end interface\n\
contains\n\
module procedure launch\n\
end procedure\n\
end submodule";
    ws.upsert_file(PathBuf::from("system.f90"), module);
    ws.upsert_file(PathBuf::from("system_subprocess.f90"), submodule);
    assert!(
        ws.diagnostics(Path::new("system_subprocess.f90"))
            .iter()
            .all(|diagnostic| !diagnostic
                .message
                .contains("host scope does not define imported name")),
        "{:?}",
        ws.diagnostics(Path::new("system_subprocess.f90"))
    );
}

#[test]
fn import_accepts_public_host_name_from_use_associated_module() {
    let mut ws = Workspace::new();
    let constants = "module constants\n\
use iso_fortran_env, only: rk => real64\n\
end module";
    let api = "module api\n\
use constants\n\
implicit none\n\
private\n\
public :: rk, zfftf\n\
interface\n\
pure subroutine zfftf(n, c)\n\
import rk\n\
integer, intent(in) :: n\n\
complex(kind=rk), intent(inout) :: c(*)\n\
end subroutine zfftf\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("constants.f90"), constants);
    ws.upsert_file(PathBuf::from("api.f90"), api);
    assert!(ws.diagnostics(Path::new("api.f90")).is_empty());
}

#[test]
fn import_accepts_late_indexed_public_host_name_from_use_associated_module() {
    let mut ws = Workspace::new();
    let constants = "module constants\n\
use iso_fortran_env, only: rk => real64\n\
end module";
    let api = "module api\n\
use constants\n\
implicit none\n\
private\n\
public :: rk, zfftf\n\
interface\n\
pure subroutine zfftf(n, c)\n\
import rk\n\
integer, intent(in) :: n\n\
complex(kind=rk), intent(inout) :: c(*)\n\
end subroutine zfftf\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("api.f90"), api);
    ws.upsert_file(PathBuf::from("constants.f90"), constants);
    assert!(ws.diagnostics(Path::new("api.f90")).is_empty());
}

#[test]
fn unresolved_host_use_does_not_cascade_import_diagnostics() {
    let mut ws = Workspace::new();
    let api = "module api\n\
use missing_kind\n\
implicit none\n\
private\n\
public :: rk, zfftf\n\
interface\n\
pure subroutine zfftf(n, c)\n\
import rk\n\
integer, intent(in) :: n\n\
complex(kind=rk), intent(inout) :: c(*)\n\
end subroutine zfftf\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("api.f90"), api);
    let diagnostics = ws.diagnostics(Path::new("api.f90"));
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic
                .message
                .contains("host scope does not define imported name"))
            .count(),
        0
    );
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("missing_kind")));
}

#[test]
fn declared_types_accept_intrinsic_module_types() {
    let mut ws = Workspace::new();
    let src = "module capi\n\
use iso_c_binding, only: c_ptr\n\
type(c_ptr) :: udata\n\
end module";
    ws.upsert_file(PathBuf::from("capi.f90"), src);
    assert!(ws.diagnostics(Path::new("capi.f90")).is_empty());
}

#[test]
fn declared_types_accept_intrinsic_type_wrappers() {
    let mut ws = Workspace::new();
    let src = "program app\n\
type(integer) :: i\n\
type(real) :: x\n\
type(character(len=:)), allocatable :: title\n\
type(character(len=*)), parameter :: fmt = '(*(g0))'\n\
end program";
    ws.upsert_file(PathBuf::from("intrinsic_type_wrappers.f90"), src);
    assert!(ws
        .diagnostics(Path::new("intrinsic_type_wrappers.f90"))
        .is_empty());
}

#[test]
fn doubleprecision_declarations_satisfy_dummy_arguments() {
    let parsed = ParsedFile::parse(
        "doubleprecision.f90",
        "subroutine convert(chars, valu)\n\
implicit none\n\
character(len=*), intent(in) :: chars\n\
doubleprecision, intent(out) :: valu\n\
end subroutine",
    );
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

#[test]
fn import_none_does_not_require_host_names() {
    let mut ws = Workspace::new();
    let src = "module m\n\
interface\n\
import, none\n\
subroutine s(x)\n\
real :: x\n\
end subroutine\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("iface.f90"), src);
    assert!(ws.diagnostics(Path::new("iface.f90")).is_empty());
}

#[test]
fn reports_host_types_not_imported_in_interfaces() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: known\n\
end type\n\
interface\n\
subroutine s(x)\n\
type(known) :: x\n\
end subroutine\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("iface.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("iface.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("Object \"known\" not imported in interface"));
}

#[test]
fn module_procedure_prototypes_do_not_require_import_for_host_types() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: known\n\
end type\n\
interface operator(+)\n\
pure module function add_known(lhs, rhs) result(value)\n\
type(known), intent(in) :: lhs\n\
type(known), intent(in) :: rhs\n\
type(known) :: value\n\
end function add_known\n\
end interface operator(+)\n\
end module";
    ws.upsert_file(PathBuf::from("module_proto.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("module_proto.f90"));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn interface_prototype_body_import_satisfies_host_type_use() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: rk_class\n\
end type\n\
interface\n\
subroutine deriv_func(me)\n\
import :: rk_class\n\
class(rk_class), intent(inout) :: me\n\
end subroutine\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("iface.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("iface.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("not imported in interface")),
        "{diagnostics:?}"
    );
}

#[test]
fn operator_interface_implementations_use_later_host_types_without_import() {
    let mut ws = Workspace::new();
    let src = "module colors\n\
implicit none\n\
private\n\
interface operator(+)\n\
module procedure add_color\n\
end interface\n\
type :: color_code\n\
integer :: style\n\
end type\n\
contains\n\
function add_color(lhs, rhs) result(code)\n\
type(color_code), intent(in) :: lhs\n\
type(color_code), intent(in) :: rhs\n\
type(color_code) :: code\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("colors.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("colors.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("not imported in interface")),
        "{diagnostics:?}"
    );
}

#[test]
fn constructor_interface_named_like_type_does_not_capture_type_members() {
    let mut ws = Workspace::new();
    let src = "module colors\n\
implicit none\n\
type :: color_code\n\
integer :: style\n\
end type\n\
type :: color_output\n\
type(color_code) :: reset\n\
type(color_code) :: bold\n\
end type\n\
interface color_output\n\
module procedure new_color_output\n\
end interface\n\
contains\n\
function new_color_output() result(new)\n\
type(color_output) :: new\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("colors.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("colors.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("not imported in interface")),
        "{diagnostics:?}"
    );
}

#[test]
fn accepts_imported_and_interface_local_declared_types() {
    let mut ws = Workspace::new();
    let imported = "module m\n\
type :: known\n\
end type\n\
interface\n\
import, only: known\n\
subroutine s(x)\n\
type(known) :: x\n\
end subroutine\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("imported_iface.f90"), imported);
    assert!(ws.diagnostics(Path::new("imported_iface.f90")).is_empty());

    let local = "module m\n\
interface\n\
type :: local_t\n\
end type\n\
subroutine s(x)\n\
type(local_t) :: x\n\
end subroutine\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("local_iface.f90"), local);
    assert!(ws.diagnostics(Path::new("local_iface.f90")).is_empty());
}

#[test]
fn reports_unresolved_declared_derived_types() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: known\n\
end type\n\
type(known) :: ok\n\
type(missing) :: bad\n\
class(*) :: any\n\
contains\n\
subroutine s()\n\
type(known) :: local_ok\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    eprintln!("{diagnostics:?}");
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("missing"));
    assert!(diagnostics[0].message.contains("bad"));
}

#[test]
fn unresolved_use_only_types_do_not_cascade_declared_type_diagnostics() {
    let mut ws = Workspace::new();
    let src = "module tests\n\
use testdrive, only: error_type, unittest_type\n\
implicit none\n\
contains\n\
subroutine collect(testsuite)\n\
type(unittest_type), allocatable, intent(out) :: testsuite(:)\n\
end subroutine\n\
subroutine run(error)\n\
type(error_type), allocatable, intent(out) :: error\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("tests.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("tests.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("testdrive"));
}

#[test]
fn declared_types_resolve_through_public_module_reexports() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("impl.f90"),
        "module impl\n\
implicit none\n\
private\n\
public :: config_t\n\
type :: config_t\n\
integer :: value\n\
end type\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("api.f90"),
        "module api\n\
use impl, only: config_t\n\
implicit none\n\
private\n\
public :: config_t\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use api, only: config_t\n\
implicit none\n\
type(config_t) :: cfg\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("declared type `config_t`")),
        "{diagnostics:?}"
    );
}

#[test]
fn partially_indexed_whole_module_use_suppresses_declared_type_cascades() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("sparse.f90"),
        "module sparse\nuse missing_impl\nend module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use sparse\n\
type(coo_type) :: coo\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("declared type `coo_type`")),
        "{diagnostics:?}"
    );
}

#[test]
fn resolves_declared_types_imported_from_used_modules() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("types.f90"),
        "module types\ntype :: vector\nend type\nend module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse types, only: vector\ntype(vector) :: v\nend program",
    );
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
}

#[test]
fn resolves_declared_types_imported_with_use_rename() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("types.f90"),
        "module types\n\
implicit none\n\
private\n\
public :: abstract_lexer\n\
type, abstract :: abstract_lexer\n\
end type\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "module app\n\
use types, only: toml_lexer => abstract_lexer\n\
implicit none\n\
contains\n\
subroutine parse(lexer)\n\
class(toml_lexer), intent(inout) :: lexer\n\
end subroutine\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("declared type `toml_lexer`")),
        "{diagnostics:?}"
    );
}

#[test]
fn records_type_inheritance_and_type_bound_procedures() {
    let parsed = ParsedFile::parse(
        "types.f90",
        "module m\ntype, abstract :: base\ncontains\nprocedure(draw_iface), deferred, pass(self) :: draw\nend type\ntype, extends(base) :: circle\ncontains\nprocedure :: draw => draw_circle\ngeneric, public :: render => draw\nend type\nend module",
    );
    let base = parsed.symbols.iter().find(|s| s.name == "base").unwrap();
    assert!(base.is_abstract);
    let circle = parsed.symbols.iter().find(|s| s.name == "circle").unwrap();
    assert_eq!(circle.extends.as_deref(), Some("base"));
    let draw = parsed
        .symbols
        .iter()
        .find(|s| s.name == "draw" && s.scope.ends_with(&["circle".to_string()]))
        .unwrap();
    assert_eq!(draw.kind, SymbolKind::Method);
    assert_eq!(draw.binding_target.as_deref(), Some("draw_circle"));
    let deferred = parsed
        .symbols
        .iter()
        .find(|s| s.name == "draw" && s.scope.ends_with(&["base".to_string()]))
        .unwrap();
    assert!(deferred.is_deferred);
    assert_eq!(deferred.pass_arg.as_deref(), Some("self"));
    assert_eq!(parsed.generic_bindings.len(), 1);
    assert_eq!(parsed.generic_bindings[0].name, "render");
    assert_eq!(parsed.generic_bindings[0].procedures, vec!["draw"]);
}

#[test]
fn records_legacy_derived_type_definition_without_double_colon() {
    let parsed = ParsedFile::parse(
        "types.f90",
        "module m\n\
type entry_ptr\n\
type(entry), pointer :: target => null()\n\
end type entry_ptr\n\
type other_ptr\n\
type(entry), pointer :: target => null()\n\
end type other_ptr\n\
end module",
    );

    let entry_ptr = parsed
        .symbols
        .iter()
        .find(|s| s.name == "entry_ptr")
        .unwrap();
    assert_eq!(entry_ptr.kind, SymbolKind::Type);
    let target_scopes: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|s| s.name == "target")
        .map(|s| s.scope.clone())
        .collect();
    assert_eq!(
        target_scopes,
        vec![
            vec!["m".to_string(), "entry_ptr".to_string()],
            vec!["m".to_string(), "other_ptr".to_string()]
        ]
    );
    assert!(parsed
        .diagnostics
        .iter()
        .all(|diagnostic| !diagnostic.message.contains("already defined")));
}

#[test]
fn procedure_pointer_components_are_not_type_bound_methods() {
    let parsed = ParsedFile::parse(
        "types.f90",
        "module m\n\
abstract interface\n\
subroutine callback()\n\
end subroutine\n\
end interface\n\
type worker\n\
procedure(callback), pointer, nopass :: hook => null()\n\
contains\n\
procedure :: run => run_worker\n\
end type\n\
contains\n\
subroutine run_worker(self)\n\
type(worker) :: self\n\
end subroutine\n\
end module",
    );

    let hook = parsed.symbols.iter().find(|s| s.name == "hook").unwrap();
    assert_eq!(hook.kind, SymbolKind::Variable);
    let run = parsed.symbols.iter().find(|s| s.name == "run").unwrap();
    assert_eq!(run.kind, SymbolKind::Method);
}

#[test]
fn use_only_accepts_public_generic_interface_reexport_chain() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("leaf.f90"),
        "module leaf\n\
implicit none\n\
private\n\
public :: get_value\n\
interface get_value\n\
module procedure get_value_integer\n\
end interface\n\
contains\n\
subroutine get_value_integer(value)\n\
integer, intent(out) :: value\n\
end subroutine\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("middle.f90"),
        "module middle\n\
use leaf, only: get_value\n\
implicit none\n\
private\n\
public :: get_value\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("api.f90"),
        "module api\n\
use middle, only: get_value\n\
implicit none\n\
public\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use api, only: get_value\n\
implicit none\n\
integer :: value\n\
call get_value(value)\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("does not export")),
        "{diagnostics:?}"
    );
}

#[test]
fn use_only_accepts_public_partial_reexport_chain_without_leaf_module() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("middle.f90"),
        "module middle\n\
use leaf, only: get_value\n\
implicit none\n\
private\n\
public :: get_value\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("api.f90"),
        "module api\n\
use middle, only: get_value\n\
implicit none\n\
public\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use api, only: get_value\n\
end program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("does not export")),
        "{diagnostics:?}"
    );
}

#[test]
fn use_only_accepts_public_operator_interfaces() {
    let mut ws = Workspace::new();
    let module = "module ops\n\
implicit none\n\
private\n\
public :: operator(+), operator(//)\n\
interface operator(+)\n\
module procedure add_int\n\
end interface\n\
interface operator(//)\n\
module procedure concat_int\n\
end interface\n\
contains\n\
integer function add_int(a, b)\n\
integer :: a, b\n\
add_int = a + b\n\
end function\n\
integer function concat_int(a, b)\n\
integer :: a, b\n\
concat_int = a + b\n\
end function\n\
end module";
    let app = "program app\n\
use ops, only: &\n\
& operator(//), operator(+)\n\
end program";
    ws.upsert_file(PathBuf::from("ops.f90"), module);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("does not export")),
        "{diagnostics:?}"
    );
}

#[test]
fn records_type_bound_operator_and_assignment_generics() {
    let parsed = ParsedFile::parse(
        "types.f90",
        "module m\n\
type :: vector\n\
contains\n\
procedure :: add => add_vector\n\
procedure :: assign => assign_vector\n\
generic :: operator(+) => add\n\
generic :: assignment(=) => assign\n\
end type\n\
end module",
    );

    assert_eq!(parsed.generic_bindings.len(), 2);
    assert_eq!(parsed.generic_bindings[0].name, "operator(+)");
    assert_eq!(
        parsed.generic_bindings[0].kind,
        GenericBindingKind::Operator
    );
    assert_eq!(parsed.generic_bindings[0].procedures, vec!["add"]);
    assert_eq!(parsed.generic_bindings[1].name, "assignment(=)");
    assert_eq!(
        parsed.generic_bindings[1].kind,
        GenericBindingKind::Assignment
    );
    assert_eq!(parsed.generic_bindings[1].procedures, vec!["assign"]);
}

#[test]
fn reports_unimplemented_inherited_deferred_type_bound_procedures() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
type, extends(shape) :: circle\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("Deferred procedure `draw`"));
    assert!(diagnostics[0].message.contains("circle"));
}

#[test]
fn provides_quickfix_for_unimplemented_deferred_type_bound_procedures() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
type, extends(shape) :: circle\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let actions = ws.code_actions(Path::new("types.f90"));
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, "quickfix");
    assert!(actions[0].title.contains("circle"));
    assert_eq!(actions[0].edits.len(), 1);
    assert_eq!(actions[0].edits[0].range.start.line, 6);
    assert_eq!(
        actions[0].edits[0].new_text,
        "contains\n  procedure :: draw => draw\n"
    );
}

#[test]
fn quickfix_for_unimplemented_deferred_type_bound_procedures_reuses_contains() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
type, extends(shape) :: circle\n\
contains\n\
generic :: render => draw\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let actions = ws.code_actions(Path::new("types.f90"));
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].edits[0].range.start.line, 8);
    assert_eq!(
        actions[0].edits[0].new_text,
        "  procedure :: draw => draw\n"
    );
}

#[test]
fn accepts_concrete_overrides_for_inherited_deferred_type_bound_procedures() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
type, extends(shape) :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self)\n\
class(circle) :: self\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    eprintln!("{:?}", ws.diagnostics(Path::new("types.f90")));
    assert!(ws.diagnostics(Path::new("types.f90")).is_empty());
}

#[test]
fn reports_fortls_style_missing_direct_overrides_from_used_parent() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("base.f90"),
        "module base\n\
type, abstract :: abstract_lexer\n\
contains\n\
procedure(next_iface), deferred :: next\n\
procedure(extract_iface), deferred :: extract_string\n\
end type\n\
type, extends(abstract_lexer) :: toml_lexer\n\
contains\n\
procedure :: next\n\
procedure :: extract_string\n\
end type\n\
abstract interface\n\
subroutine next_iface(self)\n\
import :: abstract_lexer\n\
class(abstract_lexer) :: self\n\
end subroutine\n\
subroutine extract_iface(self)\n\
import :: abstract_lexer\n\
class(abstract_lexer) :: self\n\
end subroutine\n\
end interface\n\
contains\n\
subroutine next(self)\n\
class(toml_lexer) :: self\n\
end subroutine\n\
subroutine extract_string(self)\n\
class(toml_lexer) :: self\n\
end subroutine\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("test_lexer.f90"),
        "module test_lexer\n\
use base, only: toml_lexer\n\
type, extends(toml_lexer) :: mocked_lexer\n\
contains\n\
procedure :: next\n\
end type\n\
contains\n\
subroutine next(self)\n\
class(mocked_lexer) :: self\n\
end subroutine\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("test_lexer.f90"));
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.message == "deferred procedure \"extract_string\" not implemented"),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("\"next\"")),
        "{diagnostics:?}"
    );
}

#[test]
fn unresolved_parent_module_uses_suppress_deferred_override_cascades() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("base.f90"),
        "module base\n\
use missing_api, only: toml_table\n\
type, abstract :: serializable_t\n\
contains\n\
procedure(dump_iface), deferred :: dump_to_toml\n\
procedure(load_iface), deferred :: load_from_toml\n\
end type\n\
type, extends(serializable_t) :: dependency_tree_t\n\
contains\n\
procedure :: dump_to_toml\n\
procedure :: load_from_toml\n\
end type\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("test.f90"),
        "module test\n\
use base, only: dependency_tree_t\n\
type, extends(dependency_tree_t) :: mock_dependency_tree_t\n\
contains\n\
procedure :: resolve_dependency\n\
end type\n\
contains\n\
subroutine resolve_dependency(self)\n\
class(mock_dependency_tree_t) :: self\n\
end subroutine\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("test.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("deferred procedure")),
        "{diagnostics:?}"
    );
}

#[test]
fn reports_unresolved_type_bound_procedure_targets() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: circle\n\
contains\n\
procedure :: draw => draw_missing\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("draw"));
    assert!(diagnostics[0].message.contains("draw_missing"));
}

#[test]
fn reports_type_bound_procedure_interface_argument_mismatch() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer :: color\n\
end subroutine\n\
end interface\n\
type :: circle\n\
contains\n\
procedure(draw_iface) :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self)\n\
class(circle) :: self\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("does not match interface `draw_iface`"));
}

#[test]
fn accepts_type_bound_procedure_matching_explicit_interface() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer :: color\n\
end subroutine\n\
end interface\n\
type :: circle\n\
contains\n\
procedure(draw_iface) :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert!(diagnostics.is_empty());
}

#[test]
fn accepts_type_bound_procedure_targets_declared_in_host_interface() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type map\n\
contains\n\
procedure :: init => init_map\n\
end type\n\
interface\n\
module subroutine init_map(self)\n\
type(map) :: self\n\
end subroutine\n\
end interface\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    assert!(ws.diagnostics(Path::new("m.f90")).is_empty());
}

#[test]
fn accepts_type_bound_targets_declared_as_typed_module_functions() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type process_type\n\
contains\n\
procedure :: is_running => process_is_running\n\
end type\n\
interface is_running\n\
logical module function process_is_running(process) result(is_running)\n\
class(process_type), intent(inout) :: process\n\
end function process_is_running\n\
end interface is_running\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    assert!(ws.diagnostics(Path::new("m.f90")).is_empty());
}

#[test]
fn procedure_definition_lines_are_not_validated_as_calls() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type map\n\
contains\n\
procedure :: calls\n\
end type\n\
contains\n\
pure function calls(self)\n\
type(map) :: self\n\
integer :: calls\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("m.f90"), src);

    assert!(ws.diagnostics(Path::new("m.f90")).is_empty());
}

#[test]
fn accepts_type_bound_procedure_with_descendant_passed_object() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: shape\n\
end type\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(shape) :: self\n\
integer :: color\n\
end subroutine\n\
end interface\n\
type, extends(shape) :: circle\n\
contains\n\
procedure(draw_iface) :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert!(diagnostics.is_empty());
}

#[test]
fn reports_type_bound_procedure_unrelated_passed_object_mismatch() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: shape\n\
end type\n\
type :: square\n\
end type\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(shape) :: self\n\
integer :: color\n\
end subroutine\n\
end interface\n\
type, extends(shape) :: circle\n\
contains\n\
procedure(draw_iface) :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(square) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("does not match interface `draw_iface`"));
}

#[test]
fn reports_type_bound_procedure_interface_dummy_type_mismatch() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer, intent(in) :: color\n\
end subroutine\n\
end interface\n\
type :: circle\n\
contains\n\
procedure(draw_iface) :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
real, intent(in) :: color\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("does not match interface `draw_iface`"));
}

#[test]
fn reports_type_bound_procedure_interface_dummy_attribute_mismatch() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer, intent(in), optional :: color\n\
end subroutine\n\
end interface\n\
type :: circle\n\
contains\n\
procedure(draw_iface) :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer, intent(out), optional :: color\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("does not match interface `draw_iface`"));
}

#[test]
fn reports_type_bound_function_interface_result_type_mismatch() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
function area_iface(self) result(value)\n\
class(*) :: self\n\
real :: value\n\
end function\n\
end interface\n\
type :: circle\n\
contains\n\
procedure(area_iface) :: area => circle_area\n\
end type\n\
contains\n\
function circle_area(self) result(value)\n\
class(circle) :: self\n\
integer :: value\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("does not match interface `area_iface`"));
}

#[test]
fn reports_type_bound_function_interface_header_result_type_mismatch() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
real function area_iface(self)\n\
class(*) :: self\n\
end function\n\
end interface\n\
type :: circle\n\
contains\n\
procedure(area_iface) :: area => circle_area\n\
end type\n\
contains\n\
integer function circle_area(self)\n\
class(circle) :: self\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("does not match interface `area_iface`"));
}

#[test]
fn accepts_type_bound_procedure_matching_explicit_interface_attributes() {
    let mut ws = Workspace::new();
    let src = "module m\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer, intent(in), optional :: color\n\
end subroutine\n\
end interface\n\
type :: circle\n\
contains\n\
procedure(draw_iface) :: draw => draw_circle\n\
end type\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer, intent(in), optional :: color\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    assert!(ws.diagnostics(Path::new("types.f90")).is_empty());
}

#[test]
fn reports_generic_bindings_that_reference_unknown_methods() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
generic :: render => draw, missing\n\
end type\n\
contains\n\
subroutine draw_circle(self)\n\
class(circle) :: self\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("render"));
    assert!(diagnostics[0].message.contains("missing"));
}

#[test]
fn reports_operator_generics_that_reference_unknown_methods() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: vector\n\
contains\n\
generic :: operator(+) => add_missing\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("types.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("operator(+)"));
    assert!(diagnostics[0].message.contains("add_missing"));
}

#[test]
fn accepts_generic_bindings_that_reference_known_methods() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
generic :: render => draw\n\
end type\n\
contains\n\
subroutine draw_circle(self)\n\
class(circle) :: self\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    assert!(ws.diagnostics(Path::new("types.f90")).is_empty());
}

#[test]
fn deferred_type_bound_procedure_targets_do_not_require_implementation() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    assert!(ws.diagnostics(Path::new("types.f90")).is_empty());
}

#[test]
fn allows_abstract_children_to_inherit_deferred_type_bound_procedures() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
type, abstract, extends(shape) :: curved_shape\n\
end type\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    assert!(ws.diagnostics(Path::new("types.f90")).is_empty());
}

#[test]
fn type_bound_methods_link_to_implementation_for_hover_definition_and_signature() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
end type\n\
contains\n\
!! Draw the circle.\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);

    let hover = ws
        .hover(Path::new("types.f90"), Position::new(3, 13), src)
        .unwrap();
    assert!(hover.contains("subroutine draw(color)"));
    assert!(hover.contains("Draw the circle."));

    let definition = ws
        .definition(Path::new("types.f90"), Position::new(3, 13), src)
        .unwrap();
    assert_eq!(definition.name, "draw_circle");
    assert_eq!(definition.kind, SymbolKind::Subroutine);
    let implementation = ws
        .implementation_location(Path::new("types.f90"), Position::new(3, 13), src)
        .unwrap();
    assert_eq!(implementation.file, PathBuf::from("types.f90"));
    assert_eq!(implementation.range.start, Position::new(7, 11));

    let call_src = "program app\nuse m\nclass(circle) :: c\ncall c%draw(red)\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), call_src);
    let call_implementation = ws
        .implementation_location(Path::new("app.f90"), Position::new(3, 8), call_src)
        .unwrap();
    assert_eq!(call_implementation.file, PathBuf::from("types.f90"));
    assert_eq!(call_implementation.range.start, Position::new(7, 11));
    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(3, 14), call_src)
        .unwrap();
    assert_eq!(sig.label, "draw(color)");
    assert_eq!(sig.parameters, vec!["color"]);

    let keyword_call =
        "program app\nuse m\nclass(circle) :: c\ncall c%draw(color=red)\nend program";
    ws.upsert_file(PathBuf::from("keyword.f90"), keyword_call);
    let sig = ws
        .signature_help(Path::new("keyword.f90"), Position::new(3, 20), keyword_call)
        .unwrap();
    assert_eq!(sig.label, "draw(color)");
    assert_eq!(sig.active_parameter, 0);

    let hints = ws.inlay_hints(Path::new("app.f90"), 3, 3);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].label, "color:");
}

#[test]
fn completions_after_member_access_show_type_bound_methods_and_generics() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: shape\n\
contains\n\
procedure :: move => move_shape\n\
end type\n\
type, extends(shape) :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
procedure, private :: hidden => hidden_circle\n\
generic :: render => draw\n\
end type\n\
contains\n\
subroutine move_shape(self, dx)\n\
class(shape) :: self\n\
integer :: dx\n\
end subroutine\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
subroutine hidden_circle(self)\n\
class(circle) :: self\n\
end subroutine\n\
end module";
    let app =
        "program app\nuse m\nclass(circle) :: c\ncall c%dr\ncall c%mo\ncall c%re\nend program";
    ws.upsert_file(PathBuf::from("types.f90"), src);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let draw = ws.completions_at(Path::new("app.f90"), Position::new(3, 9), "dr");
    assert!(draw.iter().any(|item| item.label == "draw"));
    assert!(!draw.iter().any(|item| item.label == "hidden"));
    assert!(!draw.iter().any(|item| item.label == "circle"));

    let inherited = ws.completions_at(Path::new("app.f90"), Position::new(4, 9), "mo");
    assert!(inherited.iter().any(|item| item.label == "move"));

    let generic = ws.completions_at(Path::new("app.f90"), Position::new(5, 9), "re");
    assert!(generic
        .iter()
        .any(|item| { item.label == "render" && item.detail.contains("generic binding => draw") }));
}

#[test]
fn type_bound_generic_signature_help_picks_matching_argument_count() {
    let mut ws = Workspace::new();
    let src = "module m\n\
type :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
procedure :: draw_by_width => draw_circle_by_width\n\
procedure :: draw_wide => draw_circle_wide\n\
generic :: render => draw, draw_by_width, draw_wide\n\
end type\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
subroutine draw_circle_by_width(self, width)\n\
class(circle) :: self\n\
integer :: width\n\
end subroutine\n\
subroutine draw_circle_wide(self, color, width)\n\
class(circle) :: self\n\
integer :: color\n\
integer :: width\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("types.f90"), src);

    let one_arg = "program app\nuse m\nclass(circle) :: c\ncall c%render(red)\nend program";
    ws.upsert_file(PathBuf::from("one.f90"), one_arg);
    let sig = ws
        .signature_help(Path::new("one.f90"), Position::new(3, 15), one_arg)
        .unwrap();
    assert_eq!(sig.label, "draw(color)");
    assert_eq!(sig.parameters, vec!["color"]);

    let keyword_arg =
        "program app\nuse m\nclass(circle) :: c\ncall c%render(width=wide)\nend program";
    ws.upsert_file(PathBuf::from("keyword.f90"), keyword_arg);
    let sig = ws
        .signature_help(Path::new("keyword.f90"), Position::new(3, 22), keyword_arg)
        .unwrap();
    assert_eq!(sig.label, "draw_by_width(width)");
    assert_eq!(sig.parameters, vec!["width"]);

    let two_args = "program app\nuse m\nclass(circle) :: c\ncall c%render(red, width)\nend program";
    ws.upsert_file(PathBuf::from("two.f90"), two_args);
    let sig = ws
        .signature_help(Path::new("two.f90"), Position::new(3, 22), two_args)
        .unwrap();
    assert_eq!(sig.label, "draw_wide(color, width)");
    assert_eq!(sig.parameters, vec!["color", "width"]);
    assert_eq!(sig.active_parameter, 1);
}

#[test]
fn object_member_lookup_finds_inherited_type_bound_methods() {
    let mut ws = Workspace::new();
    let types = "module m\n\
type :: shape\n\
contains\n\
procedure :: draw => draw_shape\n\
end type\n\
type, extends(shape) :: circle\n\
end type\n\
contains\n\
!! Draw any shape.\n\
subroutine draw_shape(self, color)\n\
class(shape) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    let app = "program app\n\
use m, only: circle\n\
type(circle) :: c\n\
call c%draw(red)\n\
end program";
    ws.upsert_file(PathBuf::from("types.f90"), types);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let hover = ws
        .hover(Path::new("app.f90"), Position::new(3, 8), app)
        .unwrap();
    assert!(
        hover.contains("subroutine draw(color)"),
        "unexpected hover: {hover}"
    );
    assert!(hover.contains("Draw any shape."));

    let definition = ws
        .definition(Path::new("app.f90"), Position::new(3, 8), app)
        .unwrap();
    assert_eq!(definition.name, "draw_shape");

    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(3, 13), app)
        .unwrap();
    assert_eq!(sig.label, "draw(color)");
    assert_eq!(sig.parameters, vec!["color"]);
}

#[test]
fn polymorphic_class_receiver_resolves_unique_descendant_override() {
    let mut ws = Workspace::new();
    let types = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
type, extends(shape) :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
end type\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer :: color\n\
end subroutine\n\
end interface\n\
contains\n\
!! Draw a circle.\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    let app = "program app\n\
use m, only: shape\n\
class(shape) :: item\n\
call item%draw(red)\n\
end program";
    ws.upsert_file(PathBuf::from("types.f90"), types);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let hover = ws
        .hover(Path::new("app.f90"), Position::new(3, 14), app)
        .unwrap();
    assert!(
        hover.contains("subroutine draw(color)"),
        "unexpected hover: {hover}"
    );
    assert!(hover.contains("Draw a circle."));

    let definition = ws
        .definition(Path::new("app.f90"), Position::new(3, 14), app)
        .unwrap();
    assert_eq!(definition.name, "draw_circle");
    assert_eq!(definition.kind, SymbolKind::Subroutine);

    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(3, 15), app)
        .unwrap();
    assert_eq!(sig.label, "draw(color)");
    assert_eq!(sig.parameters, vec!["color"]);
}

#[test]
fn polymorphic_generic_descendant_uses_keyword_arguments_and_skips_ambiguous_hints() {
    let mut ws = Workspace::new();
    let types = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
procedure(draw_width_iface), deferred :: draw_by_width\n\
generic :: render => draw, draw_by_width\n\
end type\n\
type, extends(shape) :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
procedure :: draw_by_width => draw_circle_by_width\n\
end type\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer :: color\n\
end subroutine\n\
subroutine draw_width_iface(self, width)\n\
class(*) :: self\n\
integer :: width\n\
end subroutine\n\
end interface\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
subroutine draw_circle_by_width(self, width)\n\
class(circle) :: self\n\
integer :: width\n\
end subroutine\n\
end module";
    let app = "program app\n\
use m, only: shape\n\
class(shape) :: item\n\
call item%render(width=wide)\n\
call item%render(wide)\n\
end program";
    ws.upsert_file(PathBuf::from("types.f90"), types);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("width")),
        "{diagnostics:?}"
    );

    let hints = ws.inlay_hints(Path::new("app.f90"), 4, 4);
    assert!(hints.is_empty(), "{hints:?}");
}

#[test]
fn polymorphic_class_receiver_does_not_guess_ambiguous_descendant_override() {
    let mut ws = Workspace::new();
    let types = "module m\n\
type, abstract :: shape\n\
contains\n\
procedure(draw_iface), deferred :: draw\n\
end type\n\
type, extends(shape) :: circle\n\
contains\n\
procedure :: draw => draw_circle\n\
end type\n\
type, extends(shape) :: square\n\
contains\n\
procedure :: draw => draw_square\n\
end type\n\
abstract interface\n\
subroutine draw_iface(self, color)\n\
class(*) :: self\n\
integer :: color\n\
end subroutine\n\
end interface\n\
contains\n\
subroutine draw_circle(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
subroutine draw_square(self, color)\n\
class(square) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    let app = "program app\n\
use m, only: shape\n\
class(shape) :: item\n\
call item%draw(red)\n\
end program";
    ws.upsert_file(PathBuf::from("types.f90"), types);
    ws.upsert_file(PathBuf::from("app.f90"), app);

    let definition = ws
        .definition(Path::new("app.f90"), Position::new(3, 14), app)
        .unwrap();
    assert_eq!(definition.name, "draw");
    assert_eq!(definition.kind, SymbolKind::Method);
}

#[test]
fn provides_global_intrinsic_hover_signature_and_completion() {
    let mut ws = Workspace::new();
    let src = "program app\nx = sin(theta)\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let hover = ws
        .hover(Path::new("app.f90"), Position::new(1, 5), src)
        .unwrap();
    assert!(hover.contains("sin(x)"));
    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(1, 10), src)
        .unwrap();
    assert_eq!(sig.label, "sin(x)");
    assert_eq!(sig.parameters, vec!["x"]);
    let completions = ws.completions(Path::new("app.f90"), "si");
    assert!(completions.iter().any(|item| item.label == "sin"));
}

#[test]
fn intrinsic_signature_help_tracks_keyword_arguments() {
    let mut ws = Workspace::new();
    let src = "program app\nch = achar(65, kind=4)\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let sig = ws
        .signature_help(Path::new("app.f90"), Position::new(1, 21), src)
        .unwrap();
    assert_eq!(sig.label, "achar(i, kind=kind)");
    assert_eq!(sig.active_parameter, 1);
}

#[test]
fn inlay_hints_cover_intrinsics_and_skip_named_arguments() {
    let mut ws = Workspace::new();
    let src = "program app\nch = achar(65, kind=4)\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let hints = ws.inlay_hints(Path::new("app.f90"), 1, 1);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].label, "i:");
}

#[test]
fn diagnostics_report_bad_intrinsic_call_arguments() {
    let mut ws = Workspace::new();
    let src = "program app\n\
ch = achar(65, kind=4, extra=1)\n\
end program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("no argument named `extra`"));
}

#[test]
fn diagnostics_accept_variadic_and_reduction_intrinsic_forms() {
    let mut ws = Workspace::new();
    let src = "program app\n\
logical :: ok(2)\n\
real :: a, b, c, mismatch\n\
if (all(ok)) mismatch = max(mismatch, abs(a - b), abs(b - c))\n\
end program";
    ws.upsert_file(PathBuf::from("intrinsics.f90"), src);
    assert!(ws.diagnostics(Path::new("intrinsics.f90")).is_empty());
}

#[test]
fn diagnostics_accept_typed_array_constructors() {
    let mut ws = Workspace::new();
    let src = "program app\n\
integer, parameter :: rk = kind(1.0)\n\
type point\n\
integer :: x\n\
end type\n\
real(kind=rk) :: xs(2) = [real(kind=rk) :: 1, 2]\n\
complex(kind=rk) :: zs(1) = [complex(kind=rk) :: (1, 0)]\n\
type(point) :: pts(1) = [point(x=1) :: point(1)]\n\
end program";
    ws.upsert_file(PathBuf::from("constructors.f90"), src);
    assert!(ws.diagnostics(Path::new("constructors.f90")).is_empty());
}

#[test]
fn diagnostics_accept_merge_mask_keyword() {
    let mut ws = Workspace::new();
    let src = "program app\n\
integer :: stride_, stride\n\
stride_ = merge(stride_, stride, mask=stride == 0)\n\
stride_ = merge(stride_, stride, stride == 0)\n\
end program";
    ws.upsert_file(PathBuf::from("intrinsics.f90"), src);
    assert!(ws.diagnostics(Path::new("intrinsics.f90")).is_empty());
}

#[test]
fn diagnostics_accept_optional_only_intrinsic_subroutine_arguments() {
    let mut ws = Workspace::new();
    let src = "program app\n\
integer :: values(8), count\n\
character(len=32) :: message\n\
call date_and_time(values=values)\n\
call system_clock(count=count)\n\
call random_seed()\n\
call flush(6, iostat=count, iomsg=message)\n\
end program";
    ws.upsert_file(PathBuf::from("optional_intrinsics.f90"), src);
    assert!(ws
        .diagnostics(Path::new("optional_intrinsics.f90"))
        .is_empty());
}

#[test]
fn diagnostics_accept_parenthesized_io_statements() {
    let mut ws = Workspace::new();
    let src = "program app\n\
integer :: unit, stat\n\
character(len=32) :: file\n\
open(newunit=unit, file=file, status='old', position='append', iostat=stat)\n\
100 close(unit=unit, iostat=stat)\n\
end program";
    ws.upsert_file(PathBuf::from("io.f90"), src);
    assert!(ws.diagnostics(Path::new("io.f90")).is_empty());
}

#[test]
fn semicolon_separated_one_line_functions_export_from_modules() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("m_cli.f90"),
        "module m_cli\n\
implicit none\n\
private\n\
public :: rget\n\
contains\n\
function rget(n); real :: rget; character(len=*), intent(in) :: n; rget = 0.0; end function rget\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse m_cli, only: rget\nreal :: x\nx = rget('x')\nend program",
    );
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
}

#[test]
fn module_procedure_with_extra_spaces_stays_in_interface_scope() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("generic.f90"),
        "module generic\n\
implicit none\n\
private\n\
public :: get\n\
interface get; module  procedure get_i; end interface\n\
contains\n\
subroutine get_i(x)\n\
integer, intent(out) :: x\n\
x = 1\n\
end subroutine\n\
end module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\nuse generic, only: get\ninteger :: x\ncall get(x)\nend program",
    );
    let diagnostics = ws.diagnostics(Path::new("generic.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("procedure")),
        "{diagnostics:?}"
    );
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
}

#[test]
fn typed_module_functions_parse_as_interface_prototypes() {
    let src = "module network_mod\n\
type :: network\n\
contains\n\
procedure :: get_num_params\n\
end type\n\
interface\n\
module integer function get_num_params(self)\n\
class(network), intent(in) :: self\n\
end function get_num_params\n\
end interface\n\
end module";
    let parsed = ParsedFile::parse("network.f90", src);
    assert!(
        parsed.diagnostics.iter().all(|diag| !diag
            .message
            .contains("end statement has no matching function scope")
            && !diag
                .message
                .contains("subroutine/function definition before contains")),
        "{:?}",
        parsed.diagnostics
    );
    assert!(parsed.symbols.iter().any(|sym| {
        sym.kind == SymbolKind::Function && sym.name.eq_ignore_ascii_case("get_num_params")
    }));

    let mut ws = Workspace::new();
    ws.upsert_file(PathBuf::from("network.f90"), src);
    assert!(ws.diagnostics(Path::new("network.f90")).is_empty());
}

#[test]
fn diagnostics_accept_scalar_c_f_pointer_without_shape() {
    let mut ws = Workspace::new();
    let src = "program app\n\
use iso_c_binding, only: c_ptr, c_f_pointer\n\
type(c_ptr) :: raw\n\
integer, pointer :: value\n\
call c_f_pointer(raw, value)\n\
end program";
    ws.upsert_file(PathBuf::from("cptr.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("cptr.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `c_f_pointer`")),
        "{diagnostics:?}"
    );
}

#[test]
fn diagnostics_accept_command_argument_count_without_arguments() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("env.f90"),
        "module env\n\
contains\n\
subroutine collect()\n\
integer :: i\n\
do i = 1, command_argument_count()\n\
end do\n\
end subroutine\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("env.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `command_argument_count`")),
        "{diagnostics:?}"
    );
}

#[test]
fn diagnostics_accept_c_interface_function_calls_with_arguments() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("fs.F90"),
        "module fs\n\
use iso_c_binding, only: c_char, c_ptr, c_int, c_null_char\n\
use iso_c_binding, only: c_associated\n\
interface\n\
function c_opendir(dir) result(r) bind(c, name='c_opendir')\n\
import c_char, c_ptr\n\
character(kind=c_char), intent(in) :: dir(*)\n\
type(c_ptr) :: r\n\
end function\n\
function c_closedir(dir) result(r) bind(c, name='closedir')\n\
import c_ptr, c_int\n\
type(c_ptr), intent(in), value :: dir\n\
integer(kind=c_int) :: r\n\
end function\n\
end interface\n\
contains\n\
subroutine scan(dir)\n\
character(len=*), intent(in) :: dir\n\
type(c_ptr) :: dir_handle\n\
integer(c_int) :: r\n\
dir_handle = c_opendir(dir(1:len_trim(dir))//c_null_char)\n\
if (.not. c_associated(dir_handle)) print *, 'missing'\n\
r = c_closedir(dir_handle)\n\
print *, 'c_opendir() failed'\n\
print *, 'c_closedir() failed'\n\
end subroutine\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("fs.F90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `c_opendir`")
                && !diag.message.contains("call to `c_closedir`")
                && !diag.message.contains("call to `c_associated`")),
        "{diagnostics:?}"
    );
}

#[test]
fn diagnostics_accept_implicit_function_result_substring_assignment() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("fs.F90"),
        "module fs\n\
contains\n\
function get_dos_path(path, error)\n\
character(len=*), intent(in) :: path\n\
integer, intent(out) :: error\n\
character(len=:), allocatable :: get_dos_path\n\
integer :: last\n\
get_dos_path = trim(path)\n\
last = len_trim(get_dos_path)\n\
if (last > 1) get_dos_path = get_dos_path(1:last-1)\n\
end function\n\
end module",
    );

    let diagnostics = ws.diagnostics(Path::new("fs.F90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `get_dos_path`")),
        "{diagnostics:?}"
    );
}

#[test]
fn diagnostics_report_missing_required_intrinsic_arguments() {
    let mut ws = Workspace::new();
    let src = "program app\n\
ch = achar(kind=4)\n\
ok = achar(65)\n\
end program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("missing required argument `i`"));
}

#[test]
fn diagnostics_report_bad_type_bound_call_arguments() {
    let mut ws = Workspace::new();
    let types = "module shapes\n\
type :: circle\n\
contains\n\
procedure :: draw\n\
end type\n\
contains\n\
subroutine draw(self, color)\n\
class(circle) :: self\n\
integer :: color\n\
end subroutine\n\
end module";
    let app = "program app\n\
use shapes, only: circle\n\
type(circle) :: c\n\
call c%draw(color=1, shade=2)\n\
end program";
    ws.upsert_file(PathBuf::from("types.f90"), types);
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("no argument named `shade`"));
}

#[test]
fn direct_calls_to_type_bound_implementations_keep_passed_object_argument() {
    let mut ws = Workspace::new();
    let src = "module deps\n\
type :: tree\n\
contains\n\
procedure :: add_node\n\
procedure :: has_node\n\
end type\n\
contains\n\
subroutine add_node(self, node, error)\n\
class(tree), intent(inout) :: self\n\
integer, intent(in) :: node\n\
integer, intent(out) :: error\n\
if (self%has_node(node)) error = 1\n\
call add_node(self, node, error)\n\
end subroutine\n\
logical function has_node(self, node)\n\
class(tree), intent(in) :: self\n\
integer, intent(in) :: node\n\
has_node = .false.\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("deps.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("deps.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `add_node`")
                && !diag.message.contains("call to `has_node`")),
        "{diagnostics:?}"
    );
}

#[test]
fn type_bound_calls_on_array_components_do_not_fall_back_to_direct_calls() {
    let mut ws = Workspace::new();
    let src = "module features\n\
implicit none\n\
type :: feature_t\n\
contains\n\
procedure :: load_from_toml\n\
procedure :: has_cpp\n\
end type\n\
type :: collection_t\n\
type(feature_t) :: base\n\
type(feature_t), allocatable :: variants(:)\n\
contains\n\
procedure :: load_from_toml => collection_load\n\
procedure :: has_cpp => collection_has_cpp\n\
end type\n\
contains\n\
subroutine load_from_toml(self, table, error)\n\
class(feature_t), intent(inout) :: self\n\
integer, intent(in) :: table\n\
integer, intent(out) :: error\n\
end subroutine\n\
logical function has_cpp(self)\n\
class(feature_t), intent(in) :: self\n\
has_cpp = .false.\n\
end function\n\
subroutine collection_load(self, table, error)\n\
class(collection_t), intent(inout) :: self\n\
integer, intent(in) :: table\n\
integer, intent(out) :: error\n\
integer :: i\n\
call self%base%load_from_toml(table, error)\n\
call self%variants(i)%load_from_toml(table, error)\n\
end subroutine\n\
logical function collection_has_cpp(self)\n\
class(collection_t), intent(in) :: self\n\
integer :: i\n\
collection_has_cpp = self%base%has_cpp()\n\
collection_has_cpp = self%variants(i)%has_cpp()\n\
end function\n\
end module";
    ws.upsert_file(PathBuf::from("features.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("features.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("call to `load_from_toml`")
                && !diag.message.contains("call to `has_cpp`")),
        "{diagnostics:?}"
    );
}

#[test]
fn diagnostics_report_missing_required_type_bound_arguments() {
    let mut ws = Workspace::new();
    let types = "module shapes\n\
type :: circle\n\
contains\n\
procedure :: draw\n\
end type\n\
contains\n\
subroutine draw(self, color, width)\n\
class(circle) :: self\n\
integer :: color\n\
integer, optional :: width\n\
end subroutine\n\
end module";
    let app = "program app\n\
use shapes, only: circle\n\
type(circle) :: c\n\
call c%draw()\n\
call c%draw(7)\n\
end program";
    ws.upsert_file(PathBuf::from("types.f90"), types);
    ws.upsert_file(PathBuf::from("app.f90"), app);
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0]
        .message
        .contains("missing required argument `color`"));
}

#[test]
fn hover_reports_fortran_literal_types() {
    let mut ws = Workspace::new();
    let src = "program app\n\
i = 42\n\
r = 1.25d0\n\
flag = .false.\n\
name = 'Ada'\n\
end program";
    ws.upsert_file(PathBuf::from("app.f90"), src);

    let int_hover = ws
        .hover(Path::new("app.f90"), Position::new(1, 5), src)
        .unwrap();
    assert!(int_hover.contains("INTEGER"));

    let real_hover = ws
        .hover(Path::new("app.f90"), Position::new(2, 6), src)
        .unwrap();
    assert!(real_hover.contains("REAL"));

    let logical_hover = ws
        .hover(Path::new("app.f90"), Position::new(3, 9), src)
        .unwrap();
    assert!(logical_hover.contains("LOGICAL"));

    let string_hover = ws
        .hover(Path::new("app.f90"), Position::new(4, 9), src)
        .unwrap();
    assert!(string_hover.contains("CHARACTER(LEN=3)"));
}

#[test]
fn uses_fortls_intrinsic_procedure_table() {
    let mut ws = Workspace::new();
    let src = "program app\nch = achar(65)\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let hover = ws
        .hover(Path::new("app.f90"), Position::new(1, 6), src)
        .unwrap();
    assert!(hover.contains("achar(i, kind=kind)"));
    assert!(hover.contains("ASCII collating sequence"));
    let completions = ws.completions(Path::new("app.f90"), "ach");
    assert!(completions.iter().any(|item| item.label == "achar"));
}

#[test]
fn resolves_intrinsic_module_exports_for_use_only() {
    let mut ws = Workspace::new();
    let src = "program app\nuse, intrinsic :: iso_fortran_env, only: int32\ninteger(int32) :: x\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
    let completions = ws.completions(Path::new("app.f90"), "int");
    assert!(completions.iter().any(|item| item.label == "int32"));
    assert!(!completions.iter().any(|item| item.label == "int64"));
}

#[test]
fn resolves_intrinsic_module_without_space_after_use_comma() {
    let mut ws = Workspace::new();
    let src =
        "program app\nuse,intrinsic :: ieee_arithmetic, only: ieee_is_nan\nok = ieee_is_nan(x)\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
}

#[test]
fn resolves_non_intrinsic_use_qualifier_to_project_module() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("iso_fortran_env.f90"),
        "module iso_fortran_env\ninteger, parameter :: ijk = 0\nend module",
    );
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\n\
use, non_intrinsic :: iso_fortran_env\n\
stop ijk\n\
end program",
    );

    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
}

#[test]
fn resolves_intrinsic_module_renames() {
    let mut ws = Workspace::new();
    let src = "program app\nuse, intrinsic :: iso_fortran_env, only: i32 => int32\ninteger(i32) :: x\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
    let hover = ws
        .hover(Path::new("app.f90"), Position::new(2, 8), src)
        .unwrap();
    assert!(hover.contains("int32"));
    let completions = ws.completions(Path::new("app.f90"), "i3");
    assert!(completions.iter().any(|item| item.label == "i32"));
}

#[test]
fn uses_fortls_intrinsic_module_table() {
    let mut ws = Workspace::new();
    let src = "program app\nuse, intrinsic :: openacc, only: acc_get_num_devices\nn = acc_get_num_devices(0)\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    assert!(ws.diagnostics(Path::new("app.f90")).is_empty());
    let hover = ws
        .hover(Path::new("app.f90"), Position::new(2, 6), src)
        .unwrap();
    assert!(hover.contains("acc_get_num_devices(dev_type)"));
    assert!(hover.contains("module: `openacc`"));
    let completions = ws.completions(Path::new("app.f90"), "acc_get_num");
    assert!(completions
        .iter()
        .any(|item| item.label == "acc_get_num_devices"));
}

#[test]
fn reports_unknown_intrinsic_module_only_name() {
    let mut ws = Workspace::new();
    let src = "program app\nuse, intrinsic :: iso_fortran_env, only: not_a_kind\nend program";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("not_a_kind"));
}

#[test]
fn records_preprocessor_directives_definitions_and_regions() {
    let parsed = ParsedFile::parse(
        "pp.F90",
        "#define USE_FAST 1\nmodule m\n#ifdef USE_FAST\ninteger :: fast\n#else\ninteger :: slow\n#endif\n#include \"config.inc\"\nend module",
    );
    assert_eq!(
        parsed
            .preprocessor
            .iter()
            .map(|directive| directive.kind)
            .collect::<Vec<_>>(),
        vec![
            PreprocessorKind::Define,
            PreprocessorKind::Ifdef,
            PreprocessorKind::Else,
            PreprocessorKind::Endif,
            PreprocessorKind::Include,
        ]
    );
    assert_eq!(
        parsed
            .preprocessor_definitions
            .get("USE_FAST")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(parsed.preprocessor_regions.len(), 2);
    assert_eq!(parsed.includes.last().unwrap().path, "config.inc");
}

#[test]
fn hover_reports_preprocessor_definitions() {
    let mut ws = Workspace::new();
    let src = "#define LIMIT 4\n\
#define WRAP(X) call X()\n\
program app\n\
integer :: n\n\
n = LIMIT\n\
WRAP(run)\n\
end program";
    ws.upsert_file(PathBuf::from("pp.F90"), src);

    let object_hover = ws
        .hover(Path::new("pp.F90"), Position::new(4, 5), src)
        .unwrap();
    assert!(object_hover.contains("#define LIMIT 4"));

    let function_hover = ws
        .hover(Path::new("pp.F90"), Position::new(5, 1), src)
        .unwrap();
    assert!(function_hover.contains("#define WRAP (X) call X()"));
}

#[test]
fn completions_offer_preprocessor_definitions() {
    let mut ws = Workspace::new();
    let src = "#define LIMIT 4\n\
#define WRAP(X) call X()\n\
#define TEMP 1\n\
#undef TEMP\n\
program app\n\
LI\n\
end program";
    ws.upsert_file(PathBuf::from("pp.F90"), src);

    let completions = ws.completions_at(Path::new("pp.F90"), Position::new(5, 2), "LI");
    let limit = completions
        .iter()
        .find(|item| item.label == "LIMIT")
        .expect("active preprocessor definition should complete");
    assert_eq!(limit.detail, "#define LIMIT 4");
    assert_eq!(limit.kind, SymbolKind::Variable);
    assert!(limit
        .documentation
        .as_deref()
        .is_some_and(|docs| docs.contains("#define LIMIT 4")));

    let wrap = ws
        .completions_at(Path::new("pp.F90"), Position::new(5, 2), "WR")
        .into_iter()
        .find(|item| item.label == "WRAP")
        .expect("function-like preprocessor definition should complete");
    assert_eq!(wrap.detail, "#define WRAP (X) call X()");

    let temp = ws.completions_at(Path::new("pp.F90"), Position::new(5, 2), "TE");
    assert!(!temp.iter().any(|item| item.label == "TEMP"));
}

#[test]
fn references_and_rename_include_preprocessor_definitions() {
    let mut ws = Workspace::new();
    let src = "#define LIMIT 4\n\
#define OTHER 8\n\
program app\n\
integer :: n\n\
n = LIMIT\n\
end program";
    ws.upsert_file(PathBuf::from("pp.F90"), src);

    let loc = ws
        .definition_location(Path::new("pp.F90"), Position::new(4, 5), src)
        .unwrap();
    assert_eq!(loc.file, PathBuf::from("pp.F90"));
    assert_eq!(loc.range.start.line, 0);
    assert_eq!(loc.range.start.character, "#define ".len());

    let refs = ws.references(Path::new("pp.F90"), Position::new(4, 5), src);
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().any(|loc| loc.range.start.line == 0));
    assert!(refs.iter().any(|loc| loc.range.start.line == 4));

    let edits = ws
        .rename(Path::new("pp.F90"), Position::new(4, 5), src, "MAX_LIMIT")
        .unwrap();
    assert_eq!(edits.len(), 2);
    assert!(edits
        .iter()
        .all(|edit| edit.file == PathBuf::from("pp.F90") && edit.new_text == "MAX_LIMIT"));

    assert!(matches!(
        ws.rename(Path::new("pp.F90"), Position::new(4, 5), src, "OTHER"),
        Err(RenameError::ConflictingSymbol { .. })
    ));
}

#[test]
fn undefined_preprocessor_names_do_not_hover() {
    let mut ws = Workspace::new();
    let src = "#define TEMP 1\n#undef TEMP\nprogram app\ninteger :: n\nn = TEMP\nend program";
    ws.upsert_file(PathBuf::from("pp.F90"), src);
    assert!(ws
        .hover(Path::new("pp.F90"), Position::new(4, 5), src)
        .is_none());
}

#[test]
fn reports_unbalanced_preprocessor_conditionals() {
    let parsed = ParsedFile::parse("bad.F90", "#else\nmodule m\n#ifdef X\nend module");
    assert!(parsed
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("without `#if`")));
    assert!(parsed
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("unterminated")));
}

#[test]
fn reports_unterminated_scopes_but_keeps_partial_symbols() {
    let parsed = ParsedFile::parse(
        "mid_edit.f90",
        "module m\ncontains\nsubroutine work(x)\ninteger :: x\n",
    );
    assert!(parsed.symbols.iter().any(|sym| sym.name == "m"));
    assert!(parsed.symbols.iter().any(|sym| sym.name == "work"));
    assert!(parsed.symbols.iter().any(|sym| sym.name == "x"));
    assert!(parsed
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("unterminated module scope")));
    assert!(parsed
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("unterminated subroutine scope")));
}

#[test]
fn reports_half_typed_use_without_panicking() {
    let parsed = ParsedFile::parse(
        "mid_edit.f90",
        "program app\nuse, intrinsic ::\nend program",
    );
    assert!(parsed
        .diagnostics
        .iter()
        .any(|diag| diag.message.contains("invalid use statement")));
    assert!(parsed.symbols.iter().any(|sym| sym.name == "app"));
}

#[test]
fn unsupported_end_constructs_do_not_close_real_scopes() {
    let parsed = ParsedFile::parse(
        "loops.f90",
        "subroutine work()\ndo i = 1, 3\nend do\ninteger :: after\nend subroutine",
    );
    let after = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "after")
        .unwrap();
    assert_eq!(after.scope, vec!["work"]);
    assert!(parsed.diagnostics.is_empty());
}

#[test]
fn fixed_form_comment_cards_are_not_call_sites() {
    // Netlib-style prologue comments look like calls (`C  CALL DINTDY(,,,,,)`,
    // `C***TYPE DOUBLE PRECISION (RUMACH-S, ...)`) — the call checker must
    // skip fixed-form comment cards instead of diagnosing them.
    let source = [
        "      SUBROUTINE DINTDY (T, K, DKY)",
        "      DOUBLE PRECISION T, DKY",
        "      INTEGER K",
        "      RETURN",
        "      END",
        "C***TYPE      DOUBLE PRECISION (SINTDY-S, DINTDY-D)",
        "C     CALL DINTDY(,,,,,)   Provide derivatives of y",
        "C           CALL DINTDY (T, K, RWORK(21), NYH, DKY, IFLAG)",
        "      SUBROUTINE USER()",
        "      DOUBLE PRECISION T, DKY",
        "      CALL DINTDY (T, 0, DKY)",
        "      END",
        "",
    ]
    .join("\n");
    let mut ws = Workspace::new();
    ws.upsert_file("old.f", &source);
    let diagnostics = ws.diagnostics(Path::new("old.f"));
    assert!(
        diagnostics.is_empty(),
        "comment cards must not produce call diagnostics: {diagnostics:?}",
    );
}

#[test]
fn add_use_quick_fix_offers_exporting_module() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        "math.f90",
        "module math_mod
contains
integer function answer()
answer = 42
         end function
end module",
    );
    let app = "program app
  use other_mod
  implicit none
  print *, answer()
end program";
    ws.upsert_file("app.f90", app);
    // `answer` on line 3 is unresolved; math_mod exports it.
    let actions = ws.code_actions_at(Path::new("app.f90"), Position::new(3, 12), app);
    let add_use: Vec<_> = actions
        .iter()
        .filter(|action| action.title.contains("use math_mod"))
        .collect();
    assert_eq!(add_use.len(), 1, "actions: {actions:?}");
    let edit = &add_use[0].edits[0];
    // Inserts after the last existing `use`, matching its indentation.
    assert_eq!(edit.range.start.line, 2);
    assert_eq!(edit.new_text, "  use math_mod, only: answer\n");

    // A resolvable name gets no add-use action.
    let resolved = ws.code_actions_at(Path::new("math.f90"), Position::new(2, 18),
        "module math_mod\ncontains\ninteger function answer()\nanswer = 42\nend function\nend module");
    assert!(resolved.iter().all(|a| !a.title.starts_with("Add `use")));
}

#[test]
fn block_data_units_and_common_block_names_are_indexed() {
    let source = [
        "      BLOCK DATA CBLK",
        "      COMMON /SETUP/ A, B",
        "      DOUBLE PRECISION A, B",
        "      END",
        "      BLOCK DATA NAMED",
        "      COMMON /OTHER/ C",
        "      END BLOCK DATA NAMED",
        "",
    ]
    .join("\n");
    let parsed = ParsedFile::parse("bd.f", &source);
    let names: Vec<String> = parsed
        .symbols
        .iter()
        .map(|sym| sym.qualified_name())
        .collect();
    for expected in [
        "CBLK",
        "NAMED",
        "CBLK::SETUP",
        "CBLK::A",
        "NAMED::OTHER",
        "NAMED::C",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected {expected} in {names:?}",
        );
    }
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

// ── Skeletons for unimplemented fortls-port features ─────────────────────────
// Each #[ignore]d test below specifies the expected behavior of a feature that
// is not implemented yet. To pick one up: remove #[ignore], run it, implement
// until green. Keep the no-false-diagnostics stance: tolerating a construct
// silently is always acceptable; wrong errors are not.

/// TODO(codex): EQUIVALENCE — `equivalence (a, b)` aliases storage. Minimum:
/// tolerate silently (works today); full: hover/definition on `b` should note
/// the aliased `a`. This test only pins the no-false-positives floor plus
/// symbol existence for the declared variables.
#[test]
fn equivalence_statements_are_tolerated_and_members_resolve() {
    let source = [
        "      SUBROUTINE S()",
        "      DOUBLE PRECISION A(10), B(10)",
        "      EQUIVALENCE (A(1), B(1))",
        "      EQUIVALENCE (A, C)",
        "      END",
        "",
    ]
    .join("\n");
    let parsed = ParsedFile::parse("eq.f", &source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    // C is declared *by* the equivalence (implicit typing) — it should get a
    // symbol so hover/references work.
    assert!(parsed.symbols.iter().any(|sym| sym.name == "C"));
}

/// TODO(codex): statement functions — `f(x) = x * 2.0` before the first
/// executable statement defines a function-like symbol `f` local to the
/// scope. Today the line is treated as an assignment (no symbol). Signature
/// help / hover on `f` should show `f(x)`.
#[test]
fn statement_functions_get_local_function_symbols() {
    let source = [
        "      REAL FUNCTION G(Y)",
        "      REAL F, X",
        "      F(X) = X * 2.0",
        "      G = F(Y)",
        "      END",
        "",
    ]
    .join("\n");
    let parsed = ParsedFile::parse("sf.f", &source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let f = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "F" && sym.kind == SymbolKind::Function)
        .expect("statement function symbol");
    assert_eq!(f.scope, vec!["G"]);
    assert_eq!(f.args, vec!["X"]);
}

/// TODO(codex): `do concurrent` locality specs — names in `local(...)` /
/// `local_init(...)` / `shared(...)` are construct-local variables; they
/// currently get no symbols, so references/rename inside the loop miss them.
#[test]
fn do_concurrent_locality_names_are_scoped() {
    let source = "subroutine s()\n  integer :: i\n  real :: total\n\
                  do concurrent (i = 1:10) local(total)\n    total = total + i\n\
                  end do\nend subroutine";
    let parsed = ParsedFile::parse("dc.f90", source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    // The local(total) inside the construct must not be flagged as masking,
    // and ideally gets its own construct-scoped symbol.
    assert!(!parsed
        .diagnostics
        .iter()
        .any(|d| d.message.contains("masks")),);
}

/// TODO(codex): coarrays — `codimension[*]` / `real :: x[*]` declarations and
/// image-control statements (`sync all`, `sync images`, `event post`). Floor:
/// no false diagnostics; full: the codimension shows in hover/signature.
#[test]
fn coarray_declarations_are_tolerated() {
    let source = "module m\n  real :: field(10)[*]\n  integer, codimension[*] :: counter\n\
                  contains\n  subroutine step()\n    sync all\n    field(1)[1] = 0.0\n\
                  end subroutine\nend module";
    let parsed = ParsedFile::parse("ca.f90", source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert!(parsed.symbols.iter().any(|sym| sym.name == "field"));
    assert!(parsed.symbols.iter().any(|sym| sym.name == "counter"));
}

/// TODO(codex): parameterized derived types — `type :: t(k, n)` with
/// `kind`/`len` type parameters. Floor: the type + components get symbols and
/// `type(t(4, 10)) :: v` declarations resolve; no false diagnostics.
#[test]
#[ignore = "parameterized derived types not modeled yet"]
fn parameterized_derived_types_resolve() {
    let source = "module m\n  type :: matrix(k, n)\n    integer, kind :: k\n\
                  integer, len :: n\n    real(k) :: data(n, n)\n  end type\n\
                  type(matrix(4, 10)) :: small\nend module";
    let parsed = ParsedFile::parse("pdt.f90", source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert!(parsed
        .symbols
        .iter()
        .any(|sym| sym.name == "matrix" && sym.kind == SymbolKind::Type));
    assert!(parsed.symbols.iter().any(|sym| sym.name == "small"));
}

/// TODO(codex): defined I/O — `generic :: write(formatted) => write_impl` (and
/// the interface form) binds a user-defined I/O routine. Floor: parsed as a
/// generic binding without diagnostics; the bound procedure resolves.
#[test]
#[ignore = "defined-I/O generics not modeled yet"]
fn defined_io_generic_bindings_resolve() {
    let source = "module m\n  type :: t\n  contains\n\
                  generic :: write(formatted) => write_t\n\
                  procedure :: write_t\n  end type\ncontains\n\
                  subroutine write_t(self, unit, iotype, v_list, iostat, iomsg)\n\
                  class(t), intent(in) :: self\n    integer, intent(in) :: unit\n\
                  character(*), intent(in) :: iotype\n    integer, intent(in) :: v_list(:)\n\
                  integer, intent(out) :: iostat\n    character(*), intent(inout) :: iomsg\n\
                  end subroutine\nend module";
    let parsed = ParsedFile::parse("dio.f90", source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
}

/// TODO(codex): calls spanning continuation lines are invisible to the call
/// checker — `calls_on_line` sees one physical line, so a call whose argument
/// list continues on the next card is (silently) unchecked. The checker
/// should fold continuations (reuse the parser's logical-line machinery, but
/// keep diagnostic ranges anchored to the physical start line) and then
/// diagnose missing/extra args exactly like single-line calls.
#[test]
#[ignore = "continued calls are not yet checked"]
fn continued_calls_are_argument_checked() {
    let mut ws = Workspace::new();
    let source = [
        "      SUBROUTINE TAKES2(A, B)",
        "      DOUBLE PRECISION A, B",
        "      END",
        "      SUBROUTINE USER()",
        "      DOUBLE PRECISION X",
        "      CALL TAKES2 (X,",
        "     1             X, X)",
        "      END",
        "",
    ]
    .join("\n");
    ws.upsert_file("cc.f", &source);
    let diagnostics = ws.diagnostics(Path::new("cc.f"));
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("too many positional")),
        "the folded call passes 3 args to a 2-arg subroutine: {diagnostics:?}",
    );
}

#[test]
fn legacy_common_entry_namelist_are_indexed() {
    // Fixed-form legacy: COMMON members and ENTRY points become symbols.
    let parsed = ParsedFile::parse(
        "old.f",
        "      PROGRAM OLD\n      COMMON /BLK/ X, Y(10)\n      COMMON Z\n      END\n\
         \n      REAL FUNCTION F(A)\n      F = A * 2.0\n      ENTRY G(A)\n      G = A * 3.0\n      END\n",
    );
    let names: Vec<String> = parsed
        .symbols
        .iter()
        .map(|sym| sym.qualified_name())
        .collect();
    for expected in ["OLD::X", "OLD::Y", "OLD::Z", "F", "G"] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected {expected} in {names:?}",
        );
    }
    let g = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "G")
        .expect("ENTRY symbol");
    assert_eq!(
        g.kind,
        SymbolKind::Function,
        "ENTRY inherits enclosing kind"
    );
    assert!(
        g.scope.is_empty(),
        "ENTRY is a sibling of the enclosing procedure"
    );

    // COMMON members with an explicit declaration keep the declaration symbol —
    // no duplicate-definition diagnostics.
    let modern = ParsedFile::parse(
        "mixed.f90",
        "subroutine s()\n  real :: x\n  common /blk/ x, y\n  namelist /cfg/ x, y\n\
         namelist /cfg/ y\nend subroutine",
    );
    assert!(
        modern.diagnostics.is_empty(),
        "no false positives from common/namelist: {:?}",
        modern.diagnostics,
    );
    let count = |name: &str| {
        modern
            .symbols
            .iter()
            .filter(|sym| sym.name.eq_ignore_ascii_case(name))
            .count()
    };
    assert_eq!(count("x"), 1, "declared member is not duplicated");
    assert_eq!(count("y"), 1, "common-only member gets one symbol");
    assert_eq!(count("cfg"), 1, "namelist group extension stays one symbol");
}

#[test]
fn predefined_macros_drive_preprocessor_conditionals() {
    let source = "module m\n#ifdef WITH_FAST\ninteger :: fast\n#else\ninteger :: slow\n#endif\n\
                  #if API_LEVEL >= 3\ninteger :: modern\n#endif\nend module";
    // Without the build defines: the #else branch and nothing else.
    let bare = ParsedFile::parse("cond.F90", source);
    let names: Vec<_> = bare.symbols.iter().map(|sym| sym.name.as_str()).collect();
    assert!(names.contains(&"slow"));
    assert!(!names.contains(&"fast"));
    assert!(!names.contains(&"modern"));
    // With the build's -D set: the #ifdef branch and the #if arithmetic hold.
    let defines = vec![
        ("WITH_FAST".to_string(), String::new()),
        ("API_LEVEL".to_string(), "3".to_string()),
    ];
    let defined = ParsedFile::parse_with_defines("cond.F90", source, &defines);
    let names: Vec<_> = defined
        .symbols
        .iter()
        .map(|sym| sym.name.as_str())
        .collect();
    assert!(names.contains(&"fast"));
    assert!(names.contains(&"modern"));
    assert!(!names.contains(&"slow"));
}

#[test]
fn workspace_predefined_macro_change_reparses_indexed_files() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        "guarded.F90",
        "module m\n#ifdef WITH_FAST\ninteger :: fast\n#endif\nend module",
    );
    assert!(ws.workspace_symbols("fast").is_empty());
    ws.set_predefined_macros(vec![("WITH_FAST".to_string(), String::new())]);
    assert_eq!(ws.workspace_symbols("fast").len(), 1);
}

#[test]
fn preprocessor_conditionals_filter_inactive_symbols() {
    let parsed = ParsedFile::parse(
        "cond.F90",
        "#define USE_FAST 1\n#define API_VERSION 3\n#define FLAGS 0x6u\nmodule m\n#ifdef USE_FAST\ninteger :: fast\n#else\ninteger :: slow\n#endif\n#if defined(USE_FAST) && USE_FAST == 1\ninteger :: guarded\n#endif\n#if API_VERSION >= 3 && API_VERSION < 4\ninteger :: versioned\n#else\ninteger :: old_version\n#endif\n#if ((FLAGS & 2) != 0) && (API_VERSION + 1 == 4) && (1 << 3) == 010 && (~0 != 0) && ('A' == 65) && ('\\n' == 10) && (0b1010 % 4 == 2)\ninteger :: numeric_guard\n#else\ninteger :: numeric_fallback\n#endif\nend module",
    );
    let names: Vec<_> = parsed.symbols.iter().map(|sym| sym.name.as_str()).collect();
    assert!(names.contains(&"fast"));
    assert!(names.contains(&"guarded"));
    assert!(names.contains(&"versioned"));
    assert!(names.contains(&"numeric_guard"));
    assert!(!names.contains(&"slow"));
    assert!(!names.contains(&"old_version"));
    assert!(!names.contains(&"numeric_fallback"));
}

#[test]
fn preprocessor_directives_inside_continued_statements_stay_balanced() {
    let parsed = ParsedFile::parse(
        "iface.F90",
        r#"module fpm_os
interface
function chdir_(path) result(stat) &
#ifndef _WIN32
    bind(C, name="chdir")
#else
    bind(C, name="_chdir")
#endif
end function chdir_
end interface
end module fpm_os"#,
    );
    let messages: Vec<_> = parsed
        .diagnostics
        .iter()
        .map(|diag| diag.message.as_str())
        .collect();
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("without `#if`")),
        "{messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("unterminated preprocessor")),
        "{messages:?}"
    );
}

#[test]
fn preprocessor_expands_function_like_macros_before_parsing() {
    let parsed = ParsedFile::parse(
        "macro.F90",
        "# define WRAP(PROCEDURE) PROCEDURE , wrap_/**/PROCEDURE\nmodule m\ninterface set\nmodule procedure WRAP(abc)\nend interface\ncontains\nsubroutine abc()\nend subroutine\nsubroutine wrap_abc()\nend subroutine\nend module",
    );
    let interface = parsed.symbols.iter().find(|sym| sym.name == "set").unwrap();
    assert_eq!(interface.kind, SymbolKind::Interface);
    let interface_scope = vec!["m".to_string(), "set".to_string()];
    let module_procedures: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|sym| sym.scope == interface_scope)
        .map(|sym| sym.name.as_str())
        .collect();
    assert_eq!(module_procedures, vec!["abc", "wrap_abc"]);
}

#[test]
fn preprocessor_include_definitions_expand_later_source() {
    let tmp = std::env::temp_dir().join(format!(
        "fortran-lsp-macro-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("macro.F90");
    std::fs::write(
        tmp.join("macros.inc"),
        "# ifndef USE_WRAP\n#   define WRAP(PROCEDURE) PROCEDURE\n# else\n#   define WRAP(PROCEDURE) PROCEDURE , wrap_/**/PROCEDURE\n# endif\n",
    )
    .unwrap();
    let parsed = ParsedFile::parse(
        &path,
        "#include \"macros.inc\"\nmodule m\ninterface set\nmodule procedure WRAP(abc)\nend interface\ncontains\nsubroutine abc()\nend subroutine\nend module",
    );
    let interface = parsed.symbols.iter().find(|sym| sym.name == "set").unwrap();
    assert_eq!(interface.kind, SymbolKind::Interface);
    let interface_scope = vec!["m".to_string(), "set".to_string()];
    let module_procedures: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|sym| sym.scope == interface_scope)
        .map(|sym| sym.name.as_str())
        .collect();
    assert_eq!(module_procedures, vec!["abc"]);
    assert!(
        parsed
            .diagnostics
            .iter()
            .all(|diag| !diag.message.contains("WRAP")),
        "{:?}",
        parsed.diagnostics
    );
    std::fs::remove_dir_all(tmp).ok();
}

#[test]
fn preprocessor_includes_splice_continued_procedure_arguments_and_declarations() {
    let tmp = std::env::temp_dir().join(format!(
        "fortran-lsp-include-splice-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("splice.F90");
    std::fs::write(
        tmp.join("args.inc"),
        "! dummy arguments\n!\n\nverbose,&\npath_separator&\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("decls.inc"),
        "logical, intent(in), optional :: verbose\ncharacter(len=1), intent(in), optional :: path_separator\n",
    )
    .unwrap();

    let parsed = ParsedFile::parse(
        &path,
        "module m\n\
implicit none\n\
contains\n\
subroutine initialize(me,&\n\
#include \"args.inc\"\n\
)\n\
integer, intent(inout) :: me\n\
#include \"decls.inc\"\n\
end subroutine\n\
end module",
    );

    let procedure = parsed
        .symbols
        .iter()
        .find(|sym| sym.name == "initialize")
        .unwrap();
    assert_eq!(procedure.args, vec!["me", "verbose", "path_separator"]);
    assert!(parsed.diagnostics.iter().all(|diag| {
        !diag
            .message
            .contains("No matching declaration found for argument")
            && !diag
                .message
                .contains("with INTENT keyword not found in argument list")
    }));
    std::fs::remove_dir_all(tmp).ok();
}

#[test]
fn workspace_diagnostics_skip_preprocessor_include_ranges_outside_wrapper() {
    let tmp = std::env::temp_dir().join(format!(
        "fortran-lsp-include-diagnostics-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let wrapper = tmp.join("wrapper.F90");
    std::fs::write(
        tmp.join("body.inc"),
        "\n\n\n\n\n\n\n\n\n\nsubroutine f()\ninteger :: x\nend subroutine\n",
    )
    .unwrap();

    let source = "module m\ninteger :: x\ncontains\n#include \"body.inc\"\nend module";
    let mut ws = Workspace::new();
    ws.upsert_file(wrapper.clone(), source);
    let diagnostics = ws.diagnostics(&wrapper);

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.range.start.line < 5),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("masks variable")),
        "{diagnostics:?}"
    );
    std::fs::remove_dir_all(tmp).ok();
}

#[test]
fn preprocessor_expands_object_like_macros_before_parsing() {
    let parsed = ParsedFile::parse(
        "macro.F90",
        "#define DECL integer :: from_macro\nprogram app\nDECL\nend program",
    );
    assert!(parsed.symbols.iter().any(|sym| sym.name == "from_macro"));
}

#[test]
fn inactive_preprocessor_includes_are_not_recorded() {
    let parsed = ParsedFile::parse(
        "cond_include.F90",
        "#if 0\n#include \"disabled.inc\"\n#else\n#include \"enabled.inc\"\n#endif\nprogram app\nend",
    );
    let paths: Vec<_> = parsed
        .includes
        .iter()
        .map(|include| include.path.as_str())
        .collect();
    assert_eq!(paths, vec!["enabled.inc"]);
}

#[test]
fn resolves_include_paths_from_file_directory() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("src/app.f90"),
        "program app\ninclude 'defs.inc'\nend",
    );
    ws.upsert_file(PathBuf::from("src/defs.inc"), "integer :: answer");
    let includes = ws.resolved_includes(Path::new("src/app.f90"));
    assert_eq!(includes.len(), 1);
    assert_eq!(
        includes[0].resolved_path.as_deref(),
        Some(Path::new("src/defs.inc"))
    );
    assert!(ws.diagnostics(Path::new("src/app.f90")).is_empty());
}

#[test]
fn resolves_include_paths_from_configured_roots() {
    let mut ws = Workspace::new();
    ws.set_include_roots([PathBuf::from("include")]);
    ws.upsert_file(
        PathBuf::from("src/app.f90"),
        "program app\ninclude 'defs.inc'\nend",
    );
    ws.upsert_file(PathBuf::from("include/defs.inc"), "integer :: answer");
    let includes = ws.resolved_includes(Path::new("src/app.f90"));
    assert_eq!(
        includes[0].resolved_path.as_deref(),
        Some(Path::new("include/defs.inc"))
    );
}

#[test]
fn resolves_existing_include_files_from_configured_roots() {
    let unique = format!(
        "fortran-lsp-include-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    let include_dir = root.join("include");
    std::fs::create_dir_all(&include_dir).unwrap();
    std::fs::write(include_dir.join("defs.inc"), "integer :: answer").unwrap();

    let mut ws = Workspace::new();
    ws.set_include_roots([include_dir]);
    ws.upsert_file(
        root.join("src/app.f90"),
        "program app\ninclude 'defs.inc'\nend",
    );
    let diagnostics = ws.diagnostics(&root.join("src/app.f90"));
    std::fs::remove_dir_all(&root).ok();

    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("defs.inc")),
        "{diagnostics:?}"
    );
}

#[test]
fn resolves_include_paths_from_indexed_project_files() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("pkg/src/lib.f90"),
        "module lib\ninclude 'parameters.f90'\nend module",
    );
    ws.upsert_file(PathBuf::from("pkg/inc/parameters.f90"), "integer :: dp");

    let includes = ws.resolved_includes(Path::new("pkg/src/lib.f90"));
    assert_eq!(
        includes[0].resolved_path.as_deref(),
        Some(Path::new("pkg/inc/parameters.f90"))
    );
    assert!(ws.diagnostics(Path::new("pkg/src/lib.f90")).is_empty());
}

#[test]
fn allows_external_mpi_include_without_local_file() {
    let mut ws = Workspace::new();
    ws.upsert_file(
        PathBuf::from("app.f90"),
        "program app\ninclude 'mpif.h'\nend program",
    );

    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert!(
        diagnostics
            .iter()
            .all(|diag| !diag.message.contains("mpif.h")),
        "{diagnostics:?}"
    );
}

#[test]
fn reports_and_hovers_unresolved_includes() {
    let mut ws = Workspace::new();
    let src = "program app\ninclude 'missing.inc'\nend";
    ws.upsert_file(PathBuf::from("app.f90"), src);
    let diagnostics = ws.diagnostics(Path::new("app.f90"));
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].message.contains("missing.inc"));
    let hover = ws
        .hover(Path::new("app.f90"), Position::new(1, 12), src)
        .unwrap();
    assert!(hover.contains("unresolved include"));
}

#[test]
fn include_symbols_are_visible_for_hover_definition_and_completion() {
    let mut ws = Workspace::new();
    let app = "program app\ninclude 'defs.inc'\nanswer = 42\nend";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    ws.upsert_file(PathBuf::from("defs.inc"), "integer :: answer");
    let sym = ws
        .definition(Path::new("app.f90"), Position::new(2, 2), app)
        .unwrap();
    assert_eq!(sym.name, "answer");
    assert_eq!(sym.file, PathBuf::from("defs.inc"));
    let hover = ws
        .hover(Path::new("app.f90"), Position::new(2, 2), app)
        .unwrap();
    assert!(hover.contains("integer :: answer"));
    let completions = ws.completions(Path::new("app.f90"), "ans");
    assert!(completions.iter().any(|item| item.label == "answer"));
}

#[test]
fn references_resolve_symbols_from_included_files() {
    let mut ws = Workspace::new();
    let app = "program app\ninclude 'defs.inc'\nanswer = answer + 1\nend";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    ws.upsert_file(PathBuf::from("defs.inc"), "integer :: answer");
    let refs = ws.references(Path::new("app.f90"), Position::new(2, 2), app);
    assert!(refs.iter().any(|loc| loc.file == PathBuf::from("defs.inc")));
    assert!(refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("app.f90") && loc.range.start.line == 2));
}

#[test]
fn include_symbols_are_grafted_into_the_include_statement_scope() {
    let mut ws = Workspace::new();
    let app = "module m\n\
contains\n\
subroutine work()\n\
include 'locals.inc'\n\
local_value = local_value + 1\n\
end subroutine\n\
subroutine other()\n\
local_value = 0\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    ws.upsert_file(PathBuf::from("locals.inc"), "integer :: local_value");

    let sym = ws
        .definition(Path::new("app.f90"), Position::new(4, 2), app)
        .unwrap();
    assert_eq!(sym.name, "local_value");
    assert_eq!(sym.file, PathBuf::from("locals.inc"));

    let refs = ws.references(Path::new("app.f90"), Position::new(4, 2), app);
    assert!(refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("locals.inc")));
    assert!(refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("app.f90") && loc.range.start.line == 4));
    assert!(!refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("app.f90") && loc.range.start.line == 7));
}

#[test]
fn nested_include_symbols_keep_the_enclosing_include_scope() {
    let mut ws = Workspace::new();
    let app = "module m\n\
contains\n\
subroutine work()\n\
include 'a.inc'\n\
nested_value = nested_value + 1\n\
end subroutine\n\
subroutine other()\n\
nested_value = 0\n\
end subroutine\n\
end module";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    ws.upsert_file(PathBuf::from("a.inc"), "include 'b.inc'");
    ws.upsert_file(PathBuf::from("b.inc"), "integer :: nested_value");

    let sym = ws
        .definition(Path::new("app.f90"), Position::new(4, 2), app)
        .unwrap();
    assert_eq!(sym.name, "nested_value");
    assert_eq!(sym.file, PathBuf::from("b.inc"));

    let refs = ws.references(Path::new("app.f90"), Position::new(4, 2), app);
    assert!(refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("app.f90") && loc.range.start.line == 4));
    assert!(!refs
        .iter()
        .any(|loc| loc.file == PathBuf::from("app.f90") && loc.range.start.line == 7));
}

#[test]
fn nested_include_symbols_are_visible() {
    let mut ws = Workspace::new();
    let app = "program app\ninclude 'a.inc'\nnested = nested + 1\nend";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    ws.upsert_file(PathBuf::from("a.inc"), "include 'b.inc'\ninteger :: top");
    ws.upsert_file(PathBuf::from("b.inc"), "integer :: nested");
    let sym = ws
        .definition(Path::new("app.f90"), Position::new(2, 1), app)
        .unwrap();
    assert_eq!(sym.name, "nested");
    assert_eq!(sym.file, PathBuf::from("b.inc"));
    let completions = ws.completions(Path::new("app.f90"), "nes");
    assert!(completions.iter().any(|item| item.label == "nested"));
}

#[test]
fn cyclic_includes_do_not_recurse_forever() {
    let mut ws = Workspace::new();
    let app = "program app\ninclude 'a.inc'\na_value = 1\nend";
    ws.upsert_file(PathBuf::from("app.f90"), app);
    ws.upsert_file(
        PathBuf::from("a.inc"),
        "include 'b.inc'\ninteger :: a_value",
    );
    ws.upsert_file(
        PathBuf::from("b.inc"),
        "include 'a.inc'\ninteger :: b_value",
    );
    let completions = ws.completions(Path::new("app.f90"), "");
    assert_eq!(
        completions
            .iter()
            .filter(|item| item.label == "a_value")
            .count(),
        1
    );
    assert_eq!(
        completions
            .iter()
            .filter(|item| item.label == "b_value")
            .count(),
        1
    );
}

fn token_type_at(
    source: &str,
    tokens: &[SemanticToken],
    line_needle: &str,
    token_needle: &str,
) -> Option<u32> {
    let (line_no, line) = source
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(line_needle))?;
    let byte_col = line.find(token_needle)?;
    let character = line[..byte_col].encode_utf16().count();
    tokens
        .iter()
        .find(|token| token.range.start.line == line_no && token.range.start.character == character)
        .map(|token| token.token_type)
}

fn next_u32(seed: &mut u32) -> u32 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    *seed
}
