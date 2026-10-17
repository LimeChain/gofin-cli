//! Compile finite privacy vocabularies from code-owned literals. Runtime input
//! never extends these sets; matching an error-like string is not sufficient.
use std::{collections::BTreeSet, fs, path::Path};
use syn::visit::Visit;

#[derive(Default)]
struct Vocabulary {
    codes: BTreeSet<String>,
    operations: BTreeSet<String>,
    stages: BTreeSet<String>,
    sources: BTreeSet<String>,
}

fn is_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("test")
            || (attr.path().is_ident("cfg")
                && matches!(&attr.meta, syn::Meta::List(list) if list.tokens.to_string() == "test"))
    })
}

impl Vocabulary {
    fn literal(&mut self, value: &str) {
        if value.len() <= 96
            && value.contains('_')
            && value
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        {
            self.codes.insert(value.into());
        }
    }

    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Group(group) => self.tokens(group.stream()),
                proc_macro2::TokenTree::Literal(literal) => {
                    if let Ok(string) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                        self.literal(&string.value());
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Vocabulary {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !is_test(&node.attrs) {
            syn::visit::visit_item_mod(self, node);
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !is_test(&node.attrs) {
            syn::visit::visit_item_fn(self, node);
        }
    }
    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.literal(&node.value());
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.tokens(node.tokens.clone());
    }
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = node.func.as_ref()
            && path
                .path
                .segments
                .iter()
                .any(|segment| segment.ident == "GoFinError")
            && path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "new")
            && let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(stage),
                ..
            })) = node.args.iter().nth(1)
        {
            let stage = stage.value();
            if stage.len() <= 64
                && stage
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || b"_-".contains(&c))
            {
                self.stages.insert(stage);
            }
        }
        syn::visit::visit_expr_call(self, node);
    }
}

fn walk(root: &Path, path: &Path, vocabulary: &mut Vocabulary) {
    println!("cargo:rerun-if-changed={}", path.display());
    let mut entries: Vec<_> = fs::read_dir(path)
        .expect("read source directory")
        .map(|entry| entry.expect("source entry").path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            walk(root, &entry, vocabulary);
        } else if entry.extension().is_some_and(|extension| extension == "rs") {
            let relative = entry
                .strip_prefix(root)
                .expect("workspace source")
                .to_string_lossy()
                .replace('\\', "/");
            vocabulary.sources.insert(relative);
            // Test fixtures never widen the vocabulary accepted from production input.
            if entry
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("test"))
            {
                continue;
            }
            let source = fs::read_to_string(&entry).expect("read Rust source");
            vocabulary.visit_file(
                &syn::parse_file(&source)
                    .unwrap_or_else(|error| panic!("{}: {error}", entry.display())),
            );
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=GOFIN_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=GOFIN_PUBLISHED_RELEASE");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut vocabulary = Vocabulary::default();
    // The checked-in catalog is verified against the broker registry by existing
    // CLI tests. This includes generated exchange operations without accepting
    // arbitrary strings merely because they share a known namespace prefix.
    let catalog = root.join("crates/gofin-cli/data/financial-catalog.json");
    println!("cargo:rerun-if-changed={}", catalog.display());
    let catalog: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(catalog).expect("read operation catalog"))
            .expect("valid operation catalog");
    for definition in catalog {
        let name = definition["name"].as_str().expect("operation name");
        assert!(
            name.len() <= 160
                && name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c)),
            "invalid catalog operation name"
        );
        vocabulary.operations.insert(name.into());
    }
    for entry in fs::read_dir(root.join("crates")).expect("workspace crates") {
        let entry = entry.expect("crate").path();
        let src = entry.join("src");
        if src.is_dir()
            && entry
                .file_name()
                .is_some_and(|name| name != "gofin-analytics")
        {
            walk(&root, &src, &mut vocabulary);
        }
    }
    vocabulary.codes.extend(
        [
            "INVALID_INPUT",
            "CLI_PARSE_ERROR",
            "PANIC",
            "UNKNOWN_ERROR",
            "INSTALL_FAILED",
            "UPDATE_FAILED",
        ]
        .map(str::to_owned),
    );
    vocabulary
        .stages
        .extend(["validation", "dispatch", "parse", "startup", "reporting"].map(str::to_owned));
    let output = [
        ("ERROR_CODES", vocabulary.codes),
        ("OPERATIONS", vocabulary.operations),
        ("ERROR_STAGES", vocabulary.stages),
        ("SOURCES", vocabulary.sources),
    ]
    .into_iter()
    .map(|(name, values)| {
        format!(
            "pub(crate) const {name}: &[&str] = &{:?};\n",
            values.into_iter().collect::<Vec<_>>()
        )
    })
    .collect::<String>();
    fs::write(
        Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("vocabulary.rs"),
        output,
    )
    .expect("write privacy vocabulary");
}
