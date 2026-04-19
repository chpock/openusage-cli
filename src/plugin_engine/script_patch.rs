use std::collections::{HashMap, HashSet};

use swc_core::common::{FileName, SourceMap, sync::Lrc};
use swc_core::ecma::ast::{
    AssignExpr, AssignTarget, BlockStmt, Decl, Expr, ExprStmt, Lit, ObjectLit, Pat, Prop, PropName,
    PropOrSpread, Script, SimpleAssignTarget, Stmt,
};
use swc_core::ecma::codegen::text_writer::JsWriter;
use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter};
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

#[derive(Debug, Clone)]
pub struct TransformResult {
    pub script: String,
    pub patched_functions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchMode {
    Wrap,
    Replace,
}

#[derive(Debug, Clone)]
struct FunctionPatchSpec {
    target: String,
    patch_function: String,
    mode: PatchMode,
}

#[derive(Debug, Clone)]
struct CompiledPatch {
    spec: FunctionPatchSpec,
    original_name: String,
    wrapper_stmts: Vec<Stmt>,
    applied: bool,
}

pub fn transform_plugin_script(
    _plugin_id: &str,
    source: &str,
    override_source: Option<&str>,
) -> Result<TransformResult, String> {
    let Some(override_source) = override_source else {
        return Ok(TransformResult {
            script: source.to_string(),
            patched_functions: vec![],
        });
    };

    let Some(patch_specs) = extract_patch_specs(override_source)? else {
        return Ok(TransformResult {
            script: source.to_string(),
            patched_functions: vec![],
        });
    };

    let cm: Lrc<SourceMap> = Default::default();
    let mut plugin_script = parse_script(&cm, "plugin.js", source)?;
    let compiled_patches = compile_patches(&cm, &patch_specs)?;

    let mut injector = FunctionPatchInjector::new(compiled_patches);
    plugin_script.visit_mut_with(&mut injector);

    let missing = injector.missing_targets();
    if !missing.is_empty() {
        return Err(format!(
            "AST patch target(s) not found in plugin script: {}",
            missing.join(", ")
        ));
    }

    let patched_functions = injector
        .patches
        .iter()
        .filter(|patch| patch.applied)
        .map(|patch| patch.spec.target.clone())
        .collect::<Vec<_>>();

    let script = emit_script(&cm, &plugin_script)?;
    Ok(TransformResult {
        script,
        patched_functions,
    })
}

fn compile_patches(
    cm: &Lrc<SourceMap>,
    patch_specs: &[FunctionPatchSpec],
) -> Result<Vec<CompiledPatch>, String> {
    let mut output = Vec::with_capacity(patch_specs.len());
    for spec in patch_specs {
        let original_name = make_internal_original_name(&spec.target);
        let wrapper_source = build_wrapper_snippet(spec, &original_name);
        let wrapper_stmts = parse_hook_script(
            cm,
            &format!("patch-wrapper-{}.js", spec.target),
            &wrapper_source,
        )?;
        output.push(CompiledPatch {
            spec: spec.clone(),
            original_name,
            wrapper_stmts,
            applied: false,
        });
    }
    Ok(output)
}

struct FunctionPatchInjector {
    patches: Vec<CompiledPatch>,
    patch_by_target: HashMap<String, usize>,
}

impl FunctionPatchInjector {
    fn new(patches: Vec<CompiledPatch>) -> Self {
        let mut patch_by_target = HashMap::new();
        for (idx, patch) in patches.iter().enumerate() {
            patch_by_target.insert(patch.spec.target.clone(), idx);
        }
        Self {
            patches,
            patch_by_target,
        }
    }

    fn missing_targets(&self) -> Vec<String> {
        self.patches
            .iter()
            .filter(|patch| !patch.applied)
            .map(|patch| patch.spec.target.clone())
            .collect()
    }

    fn try_patch_stmt(&mut self, stmt: &mut Stmt) -> Option<Vec<Stmt>> {
        let Stmt::Decl(Decl::Fn(fn_decl)) = stmt else {
            return None;
        };

        let target_name = fn_decl.ident.sym.as_ref().to_string();
        let Some(patch_idx) = self.patch_by_target.get(&target_name).copied() else {
            return None;
        };

        if self.patches[patch_idx].applied {
            return None;
        }

        let original_name = self.patches[patch_idx].original_name.clone();
        fn_decl.ident.sym = original_name.into();
        let injected = self.patches[patch_idx].wrapper_stmts.clone();
        self.patches[patch_idx].applied = true;
        Some(injected)
    }
}

impl VisitMut for FunctionPatchInjector {
    fn visit_mut_script(&mut self, script: &mut Script) {
        let mut output = Vec::with_capacity(script.body.len());
        for mut stmt in std::mem::take(&mut script.body) {
            stmt.visit_mut_with(self);
            let injected = self.try_patch_stmt(&mut stmt);
            output.push(stmt);
            if let Some(injected) = injected {
                output.extend(injected);
            }
        }
        script.body = output;
    }

    fn visit_mut_block_stmt(&mut self, block: &mut BlockStmt) {
        let mut output = Vec::with_capacity(block.stmts.len());
        for mut stmt in std::mem::take(&mut block.stmts) {
            stmt.visit_mut_with(self);
            let injected = self.try_patch_stmt(&mut stmt);
            output.push(stmt);
            if let Some(injected) = injected {
                output.extend(injected);
            }
        }
        block.stmts = output;
    }
}

fn extract_patch_specs(override_source: &str) -> Result<Option<Vec<FunctionPatchSpec>>, String> {
    if override_source.trim().is_empty() {
        return Ok(None);
    }

    let cm: Lrc<SourceMap> = Default::default();
    let override_script = parse_script(&cm, "override.js", override_source)?;
    let Some(manifest_obj) = find_ast_patch_manifest_object(&override_script) else {
        return Ok(None);
    };

    let declared_functions = collect_top_level_function_names(&override_script);
    let specs = parse_patch_specs_from_manifest(manifest_obj)?;

    if specs.is_empty() {
        return Err("__openusage_ast_patch.functions must not be empty".to_string());
    }

    for spec in &specs {
        if !declared_functions.contains(&spec.patch_function) {
            return Err(format!(
                "patch function '{}' for target '{}' is not declared at top-level in override script",
                spec.patch_function, spec.target
            ));
        }
    }

    Ok(Some(specs))
}

fn collect_top_level_function_names(script: &Script) -> HashSet<String> {
    let mut names = HashSet::new();

    for stmt in &script.body {
        match stmt {
            Stmt::Decl(Decl::Fn(fn_decl)) => {
                names.insert(fn_decl.ident.sym.as_ref().to_string());
            }
            Stmt::Decl(Decl::Var(var_decl)) => {
                for decl in &var_decl.decls {
                    if let Pat::Ident(binding) = &decl.name {
                        let Some(init) = &decl.init else {
                            continue;
                        };
                        if matches!(
                            &**init,
                            Expr::Fn(_) | Expr::Arrow(_) | Expr::Call(_) | Expr::Paren(_)
                        ) {
                            names.insert(binding.id.sym.as_ref().to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    names
}

fn find_ast_patch_manifest_object(script: &Script) -> Option<&ObjectLit> {
    script
        .body
        .iter()
        .find_map(extract_manifest_object_from_stmt)
}

fn extract_manifest_object_from_stmt(stmt: &Stmt) -> Option<&ObjectLit> {
    let Stmt::Expr(ExprStmt { expr, .. }) = stmt else {
        return None;
    };
    let Expr::Assign(assign_expr) = &**expr else {
        return None;
    };
    extract_manifest_object_from_assignment(assign_expr)
}

fn extract_manifest_object_from_assignment(assign_expr: &AssignExpr) -> Option<&ObjectLit> {
    if !is_ast_patch_assignment_target(&assign_expr.left) {
        return None;
    }

    let Expr::Object(obj) = &*assign_expr.right else {
        return None;
    };
    Some(obj)
}

fn is_ast_patch_assignment_target(target: &AssignTarget) -> bool {
    let AssignTarget::Simple(SimpleAssignTarget::Member(member)) = target else {
        return false;
    };

    let is_global_this = match &*member.obj {
        Expr::Ident(ident) => ident.sym.as_ref() == "globalThis",
        _ => false,
    };
    if !is_global_this {
        return false;
    }

    match &member.prop {
        swc_core::ecma::ast::MemberProp::Ident(ident) => {
            ident.sym.as_ref() == "__openusage_ast_patch"
        }
        swc_core::ecma::ast::MemberProp::Computed(computed) => {
            if let Expr::Lit(Lit::Str(str_lit)) = &*computed.expr {
                return str_lit.value == "__openusage_ast_patch";
            }
            false
        }
        _ => false,
    }
}

fn parse_patch_specs_from_manifest(manifest: &ObjectLit) -> Result<Vec<FunctionPatchSpec>, String> {
    let functions_expr = get_object_property_expr(manifest, "functions")
        .ok_or_else(|| "__openusage_ast_patch.functions is required".to_string())?;

    let Expr::Array(array_lit) = functions_expr else {
        return Err("__openusage_ast_patch.functions must be an array".to_string());
    };

    let mut specs = Vec::new();
    let mut seen_targets = HashSet::new();

    for (idx, element) in array_lit.elems.iter().enumerate() {
        let Some(element) = element else {
            continue;
        };

        let spec = match &*element.expr {
            Expr::Lit(Lit::Str(target_lit)) => {
                let target = target_lit.value.to_string_lossy().to_string();
                if target.trim().is_empty() {
                    return Err(format!("functions[{}] target must not be empty", idx));
                }
                FunctionPatchSpec {
                    target: target.clone(),
                    patch_function: target,
                    mode: PatchMode::Wrap,
                }
            }
            Expr::Object(obj) => parse_patch_spec_object(obj, idx)?,
            _ => {
                return Err(format!(
                    "functions[{}] must be a string or object definition",
                    idx
                ));
            }
        };

        if !seen_targets.insert(spec.target.clone()) {
            return Err(format!(
                "duplicate AST patch target in manifest: {}",
                spec.target
            ));
        }
        specs.push(spec);
    }

    Ok(specs)
}

fn parse_patch_spec_object(obj: &ObjectLit, idx: usize) -> Result<FunctionPatchSpec, String> {
    let target = get_object_property_string(obj, "target")
        .ok_or_else(|| format!("functions[{}].target is required", idx))?;
    if target.trim().is_empty() {
        return Err(format!("functions[{}].target must not be empty", idx));
    }

    let patch_function = get_object_property_string(obj, "with").unwrap_or_else(|| target.clone());
    if patch_function.trim().is_empty() {
        return Err(format!("functions[{}].with must not be empty", idx));
    }

    let mode = match get_object_property_string(obj, "mode") {
        None => PatchMode::Wrap,
        Some(mode) => match mode.as_str() {
            "wrap" => PatchMode::Wrap,
            "replace" => PatchMode::Replace,
            _ => {
                return Err(format!(
                    "functions[{}].mode must be 'wrap' or 'replace'",
                    idx
                ));
            }
        },
    };

    Ok(FunctionPatchSpec {
        target,
        patch_function,
        mode,
    })
}

fn get_object_property_expr<'a>(obj: &'a ObjectLit, key: &str) -> Option<&'a Expr> {
    for prop in &obj.props {
        let PropOrSpread::Prop(prop) = prop else {
            continue;
        };
        let Prop::KeyValue(kv) = &**prop else {
            continue;
        };
        if property_name_to_string(&kv.key).as_deref() == Some(key) {
            return Some(&kv.value);
        }
    }
    None
}

fn get_object_property_string(obj: &ObjectLit, key: &str) -> Option<String> {
    let expr = get_object_property_expr(obj, key)?;
    expr_to_string_literal(expr)
}

fn property_name_to_string(name: &PropName) -> Option<String> {
    match name {
        PropName::Ident(ident) => Some(ident.sym.as_ref().to_string()),
        PropName::Str(str_lit) => Some(str_lit.value.to_string_lossy().to_string()),
        _ => None,
    }
}

fn expr_to_string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(Lit::Str(str_lit)) => Some(str_lit.value.to_string_lossy().to_string()),
        _ => None,
    }
}

fn make_internal_original_name(target: &str) -> String {
    let mut out = String::from("__openusage_original_");
    for ch in target.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    if out
        .chars()
        .last()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        out.push('_');
    }

    out
}

fn build_wrapper_snippet(spec: &FunctionPatchSpec, original_name: &str) -> String {
    let target_name = &spec.target;
    let patch_name = &spec.patch_function;
    match spec.mode {
        PatchMode::Wrap => format!(
            r#"
function {target_name}(...__openusage_args) {{
  const __openusage_patch_fn = globalThis["{patch_name}"];
  if (typeof __openusage_patch_fn !== "function") {{
    return {original_name}(...__openusage_args);
  }}
  return __openusage_patch_fn({original_name}, ...__openusage_args);
}}
"#
        ),
        PatchMode::Replace => format!(
            r#"
function {target_name}(...__openusage_args) {{
  const __openusage_patch_fn = globalThis["{patch_name}"];
  if (typeof __openusage_patch_fn !== "function") {{
    return {original_name}(...__openusage_args);
  }}
  return __openusage_patch_fn(...__openusage_args);
}}
"#
        ),
    }
}

fn parse_script(cm: &Lrc<SourceMap>, name: &str, source: &str) -> Result<Script, String> {
    let source_file = cm.new_source_file(
        FileName::Custom(name.to_string()).into(),
        source.to_string(),
    );
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax {
            jsx: false,
            decorators: false,
            decorators_before_export: false,
            export_default_from: false,
            fn_bind: false,
            import_attributes: true,
            allow_super_outside_method: true,
            allow_return_outside_function: true,
            auto_accessors: false,
            explicit_resource_management: false,
        }),
        swc_core::ecma::ast::EsVersion::Es2022,
        StringInput::from(&*source_file),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let script = parser
        .parse_script()
        .map_err(|err| format!("failed to parse {}: {:?}", name, err))?;

    let mut diagnostics = parser.take_errors();
    if !diagnostics.is_empty() {
        let first = diagnostics.remove(0);
        return Err(format!("failed to parse {}: {:?}", name, first));
    }

    Ok(script)
}

fn parse_hook_script(cm: &Lrc<SourceMap>, name: &str, source: &str) -> Result<Vec<Stmt>, String> {
    let parsed = parse_script(cm, name, source)?;
    Ok(parsed.body)
}

fn emit_script(cm: &Lrc<SourceMap>, script: &Script) -> Result<String, String> {
    let mut buffer = Vec::new();
    {
        let mut emitter = Emitter {
            cfg: CodegenConfig::default(),
            comments: None,
            cm: cm.clone(),
            wr: JsWriter::new(cm.clone(), "\n", &mut buffer, None),
        };
        emitter
            .emit_script(script)
            .map_err(|err| format!("failed to emit transformed script: {}", err))?;
    }

    String::from_utf8(buffer).map_err(|err| format!("transformed script is not utf-8: {}", err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{Context, Runtime};
    use serde_json::Value;

    #[test]
    fn transform_injects_requested_function_wrappers() {
        let source = r#"
        (function () {
          function loadAuth(ctx) { return null; }
          function saveAuth(ctx, authState) { return !!authState; }
          globalThis.__openusage_plugin = { id: "codex", probe: function () {} };
        })();
        "#;
        let override_script = r#"
        globalThis.__openusage_ast_patch = {
          functions: [
            { target: "loadAuth", with: "patchLoadAuth", mode: "wrap" },
            { target: "saveAuth", with: "patchSaveAuth", mode: "wrap" }
          ]
        };
        function patchLoadAuth(original, ctx) { return original(ctx); }
        function patchSaveAuth(original, ctx, authState) { return original(ctx, authState); }
        "#;

        let transformed = transform_plugin_script("codex", source, Some(override_script))
            .expect("transform codex script");

        assert!(
            transformed
                .script
                .contains("function __openusage_original_loadAuth"),
            "loadAuth should be renamed"
        );
        assert!(
            transformed
                .script
                .contains("function __openusage_original_saveAuth"),
            "saveAuth should be renamed"
        );
        assert_eq!(
            transformed.patched_functions,
            vec!["loadAuth".to_string(), "saveAuth".to_string()]
        );
    }

    #[test]
    fn transform_wrappers_call_override_functions() {
        let source = r#"
        (function () {
          function loadAuth(ctx) { return null; }
          function saveAuth(ctx, authState) {
            return authState && authState.source === "file";
          }
          function probe(ctx) {
            const authState = loadAuth(ctx);
            const persisted = saveAuth(ctx, authState);
            return {
              plan: authState && authState.source ? authState.source : "none",
              lines: [ctx.line.badge({ label: "Persisted", text: String(persisted) })]
            };
          }
          globalThis.__openusage_plugin = { id: "codex", probe: probe };
        })();
        "#;
        let override_script = r#"
        globalThis.__openusage_ast_patch = {
          functions: [
            { target: "loadAuth", with: "patchLoadAuth", mode: "wrap" },
            { target: "saveAuth", with: "patchSaveAuth", mode: "wrap" }
          ]
        };
        function patchLoadAuth(original, ctx) {
          const primary = original(ctx);
          if (primary) return primary;
          return { source: "opencode", auth: { tokens: { access_token: "x" } } };
        }
        function patchSaveAuth(original, ctx, authState) {
          if (authState && authState.source === "opencode") {
            return true;
          }
          return original(ctx, authState);
        }
        "#;

        let transformed = transform_plugin_script("codex", source, Some(override_script))
            .expect("transform codex script");

        let rt = Runtime::new().expect("quickjs runtime");
        let ctx = Context::full(&rt).expect("quickjs context");

        let probe_json = ctx.with(|ctx| {
            ctx.eval::<(), _>(TEST_CTX_SCRIPT.as_bytes())
                .expect("eval test ctx");
            ctx.eval::<(), _>(transformed.script.as_bytes())
                .expect("eval transformed plugin");
            ctx.eval::<(), _>(override_script.as_bytes())
                .expect("eval override");
            ctx.eval::<String, _>(TEST_PROBE_SCRIPT.as_bytes())
                .expect("eval probe")
        });

        let parsed: Value = serde_json::from_str(&probe_json).expect("parse probe json");
        assert_eq!(parsed["plan"], Value::String("opencode".to_string()));
        assert_eq!(parsed["persisted"], Value::String("true".to_string()));
    }

    #[test]
    fn transform_is_noop_without_ast_manifest() {
        let source = "globalThis.__openusage_plugin = { id: \"mock\", probe: function() { return { lines: [] }; } };";
        let override_script = "globalThis.__openusage_override = { note: 'probe-only override' };";

        let transformed = transform_plugin_script("mock", source, Some(override_script))
            .expect("transform without manifest");
        assert_eq!(transformed.script, source);
        assert!(transformed.patched_functions.is_empty());
    }

    #[test]
    fn transform_errors_when_target_is_missing() {
        let source = r#"
        (function () {
          function probe() { return {}; }
          globalThis.__openusage_plugin = { id: "codex", probe: probe };
        })();
        "#;
        let override_script = r#"
        globalThis.__openusage_ast_patch = {
          functions: [{ target: "loadAuth", with: "patchLoadAuth", mode: "wrap" }]
        };
        function patchLoadAuth(original, ctx) { return original(ctx); }
        "#;

        let err = transform_plugin_script("codex", source, Some(override_script))
            .expect_err("expected missing target error");
        assert!(err.contains("loadAuth"));
    }

    const TEST_CTX_SCRIPT: &str = r#"
    (function () {
      globalThis.__test_ctx = {
        line: {
          badge: function(opts) {
            return { type: "badge", label: opts.label, text: opts.text };
          }
        }
      };
    })();
    "#;

    const TEST_PROBE_SCRIPT: &str = r#"
    (function () {
      const result = globalThis.__openusage_plugin.probe(globalThis.__test_ctx);
      return JSON.stringify({
        plan: result.plan,
        persisted: result.lines[0].text
      });
    })();
    "#;
}
