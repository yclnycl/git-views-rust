//! Source and diff previews share the same statically linked grammar selection.
//!
//! Keep this list in step with the GPUI Kit Tree-sitter features in Cargo.toml.
//! Unknown files remain readable as plain text; we never execute source files
//! or load an external language server to identify or highlight them.

/// GPUI Kit 0.8.1 links C# and CMake with empty highlight queries, while C++
/// and TSX get only additions to their base language. Complete these before any
/// editor or background diff is created. Matching queries are embedded in the executable.
pub(crate) fn initialize() {
    static INITIALIZED: std::sync::Once = std::sync::Once::new();
    INITIALIZED.call_once(|| {
        let registry = gpui_kit::component::highlighter::LanguageRegistry::singleton();
        for (language, query) in [
            ("csharp", include_str!("../../assets/syntax/csharp.scm")),
            ("cmake", include_str!("../../assets/syntax/cmake.scm")),
        ] {
            if let Some(mut grammar) = registry.language(language)
                && grammar.highlights.trim().is_empty()
            {
                grammar.highlights = query.to_owned().into();
                registry.register(language, &grammar);
            }
        }
        if let (Some(mut cpp), Some(c)) = (registry.language("cpp"), registry.language("c")) {
            cpp.highlights = format!("{}\n{}", c.highlights, cpp.highlights).into();
            registry.register("cpp", &cpp);
        }
        let jsx = include_str!("../../assets/syntax/jsx.scm");
        if let Some(mut javascript) = registry.language("javascript") {
            javascript.highlights = format!("{}\n{jsx}", javascript.highlights).into();
            registry.register("javascript", &javascript);
        }
        if let (Some(mut tsx), Some(typescript)) =
            (registry.language("tsx"), registry.language("typescript"))
        {
            tsx.highlights = format!("{}\n{}\n{jsx}", typescript.highlights, tsx.highlights).into();
            registry.register("tsx", &tsx);
        }
    });
}

/// Resolve a repository-relative path without relying on the host path syntax.
/// Git uses `/`, while a caller or test may also provide a Windows `\` path.
pub(super) fn language_for_path(path: &str) -> &'static str {
    let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower = filename.to_ascii_lowercase();
    match lower.as_str() {
        "cmakelists.txt" => return "cmake",
        "makefile" | "gnumakefile" | "bsdmakefile" => return "make",
        ".bashrc" | ".bash_profile" | ".bash_login" | ".bash_logout" | ".profile" | ".zshrc"
        | ".zprofile" | ".zlogin" | ".zlogout" => return "bash",
        ".clang-format" | ".clang-tidy" => return "yaml",
        ".prettierrc" | ".babelrc" | ".eslintrc" | ".jshintrc" => return "json",
        _ => {}
    }
    // Unix traditionally treats a capital .C as C++; preserve that distinction
    // while accepting upper-case extensions for all other languages.
    if filename.ends_with(".C") {
        return "cpp";
    }
    match lower.rsplit_once('.').map(|(_, extension)| extension) {
        Some("rs") => "rust",
        Some("c") => "c",
        Some(
            "h" | "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "inl" | "ipp" | "ixx"
            | "cu" | "cuh",
        ) => "cpp",
        Some("cs" | "csx") => "csharp",
        Some("java") => "java",
        Some("go") => "go",
        Some("sql") => "sql",
        Some("lua") => "lua",
        Some("cmake") => "cmake",
        Some("mk" | "mak") => "make",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "tsx",
        Some("json" | "jsonc" | "ipynb") => "json",
        Some("toml") => "toml",
        Some("md" | "markdown" | "mdown" | "mkd") => "markdown",
        Some("py" | "pyw" | "pyi") => "python",
        Some("css" | "scss") => "css",
        // HTML's grammar also provides basic tag/attribute highlighting for
        // Vue and XML-like documents, including SVG. Vue expressions and XML
        // schema semantics are not a separate language service.
        Some("html" | "htm" | "xhtml" | "vue" | "xml" | "svg" | "xsl" | "xslt" | "xsd" | "ui") => {
            "html"
        }
        Some("yaml" | "yml") => "yaml",
        Some("sh" | "bash" | "zsh" | "ksh") => "bash",
        _ => "text",
    }
}

/// Extensionless executable scripts still have a language when their first
/// line names a supported interpreter. Inspect only a bounded prefix and never
/// treat arbitrary occurrences of an interpreter name as a shebang.
pub(super) fn language_for_source(path: &str, text: &str) -> &'static str {
    let language = language_for_path(path);
    if language != "text" {
        return language;
    }
    let Some(header) = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .strip_prefix("#!")
    else {
        return "text";
    };
    let end = header
        .bytes()
        .take(257)
        .position(|byte| byte == b'\n')
        .unwrap_or(header.len().min(257));
    if end > 256 {
        return "text";
    }
    let header = &header[..end];
    let mut words = header.split_ascii_whitespace();
    let Some(executable) = words.next() else {
        return "text";
    };
    let mut interpreter = executable.rsplit('/').next().unwrap_or(executable);
    if interpreter == "env" {
        loop {
            let Some(word) = words.next() else {
                return "text";
            };
            if word == "-S" || word == "--" || word.contains('=') {
                continue;
            }
            // Do not guess unknown env options that may consume arguments.
            if word.starts_with('-') {
                return "text";
            }
            interpreter = word;
            break;
        }
    }
    match interpreter {
        "sh" | "bash" | "zsh" | "ksh" | "dash" => "bash",
        "node" | "nodejs" => "javascript",
        "python" | "python2" | "python3" => "python",
        name if name.starts_with("python3.")
            && name.len() > 8
            && name[8..].bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            "python"
        }
        "lua" | "lua5.1" | "lua5.2" | "lua5.3" | "lua5.4" | "luajit" => "lua",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::{language_for_path, language_for_source};
    use gpui_kit::component::{
        Rope,
        highlighter::{HighlightTheme, LanguageRegistry, SyntaxHighlighter},
    };

    #[test]
    fn paths_cover_cpp_project_files_unicode_and_extensionless_configs() {
        for (path, expected) in [
            ("src/中文/编辑器.CPP", "cpp"),
            (r"src\中文\EDITOR.HPP", "cpp"),
            ("native/single.C", "cpp"),
            ("native/single.c", "c"),
            ("src/Program.cs", "csharp"),
            ("src/Index.CTS", "typescript"),
            ("src/stubs.pyi", "python"),
            ("nested/CMakeLists.txt", "cmake"),
            ("nested/Makefile", "make"),
            ("~/.bashrc", "bash"),
            (".clang-format", "yaml"),
            ("images/中文.svg", "html"),
            ("folder.rs/README", "text"),
            ("folder.cmake/no-extension", "text"),
            ("unknown.filetype", "text"),
        ] {
            assert_eq!(language_for_path(path), expected, "{path}");
        }
    }

    #[test]
    fn extensionless_shebangs_are_bounded_and_preserve_explicit_extensions() {
        assert_eq!(
            language_for_source("script", "#!/usr/bin/env -S python3 -u\nprint(1)"),
            "python"
        );
        assert_eq!(
            language_for_source("script", "#!/usr/bin/python3.12\nprint(1)"),
            "python"
        );
        assert_eq!(
            language_for_source("script", "\u{feff}#!/bin/bash\necho 中文"),
            "bash"
        );
        assert_eq!(
            language_for_source("script", "#!/usr/bin/env node\nconst a = 1;"),
            "javascript"
        );
        assert_eq!(
            language_for_source("script", "#!/usr/bin/env VERSION=1 lua\nreturn 1"),
            "lua"
        );
        assert_eq!(
            language_for_source("file.rs", "#!/usr/bin/python3\nfn main() {}"),
            "rust"
        );
        assert_eq!(language_for_source("file", "ordinary python3 text"), "text");
        assert_eq!(
            language_for_source("file", "#!/usr/bin/env --unknown python3"),
            "text"
        );
        assert_eq!(
            language_for_source("file", "#!/usr/bin/python3.fake"),
            "text"
        );
        assert_eq!(
            language_for_source(
                "file",
                &format!("#!/usr/bin/env {} python3", " ".repeat(256))
            ),
            "text"
        );
    }

    #[test]
    fn every_supported_source_language_has_a_working_linked_grammar_and_colored_tokens() {
        super::initialize();
        for (path, code) in [
            ("file.rs", "fn main() { let value = 42; }"),
            ("file.c", "int main(void) { return 42; }"),
            ("file.cpp", "class Example { public: int value = 42; };"),
            ("file.cs", "public class Example { public int Value = 42; }"),
            ("file.java", "public class Example { int value = 42; }"),
            (
                "file.go",
                "package main\nfunc main() { value := 42; _ = value }",
            ),
            ("file.sql", "SELECT COUNT(*) FROM users WHERE id = 42;"),
            ("file.lua", "local value = 42\nreturn value"),
            (
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.20)\nproject(Example)",
            ),
            ("Makefile", "VALUE := 42\nall:\n\t@echo $(VALUE)\n"),
            ("file.js", "const value = 42; console.log(value);"),
            ("file.ts", "const value: number = 42;"),
            ("file.tsx", "const view = <div title=\"中文\">hello</div>;"),
            ("file.json", "{\"value\": 42, \"enabled\": true}"),
            ("file.toml", "[package]\nname = \"example\"\n"),
            (
                "file.md",
                "# Example\n**bold** [link](https://example.com)\n",
            ),
            ("file.py", "def example():\n    return 42\n"),
            ("file.css", ".card { color: #336699; }"),
            ("file.html", "<div title=\"example\">hello</div>"),
            ("file.yaml", "value: 42\nenabled: true\n"),
            ("file.sh", "#!/bin/bash\necho \"hello\"\n"),
        ] {
            let language = language_for_source(path, code);
            let grammar = LanguageRegistry::singleton().language(language).unwrap();
            assert!(grammar.has_grammar(), "{path}: grammar is not linked");
            let rope = Rope::from_str(code);
            let mut highlighter = SyntaxHighlighter::new(language);
            assert!(
                highlighter.update(None, &rope, None),
                "{path}: parse did not complete"
            );
            assert!(highlighter.tree().is_some(), "{path}: grammar/query failed");
            for theme in [
                HighlightTheme::default_dark(),
                HighlightTheme::default_light(),
            ] {
                let styles = highlighter.styles(&(0..code.len()), theme.as_ref());
                assert!(
                    styles
                        .iter()
                        .any(|(range, style)| !range.is_empty() && style.color.is_some()),
                    "{path}: no colored tokens"
                );
                assert!(
                    styles.iter().all(|(range, _)| {
                        range.start <= range.end
                            && range.end <= code.len()
                            && code.is_char_boundary(range.start)
                            && code.is_char_boundary(range.end)
                    }),
                    "{path}: a token cuts through a UTF-8 character"
                );
            }
        }
    }

    #[test]
    fn supplemented_queries_keep_real_token_categories_instead_of_one_flat_color() {
        super::initialize();
        type TokenSample<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
        let samples: &[TokenSample<'_>] = &[
            (
                "file.cpp",
                "// 中文注释\nclass Example { public: const char* text = \"hello\"; int number = 42; };",
                &[
                    ("// 中文注释", "comment"),
                    ("class", "keyword"),
                    ("Example", "type"),
                    ("\"hello\"", "string"),
                    ("42", "number"),
                ],
            ),
            (
                "file.cs",
                "// 中文注释\npublic class Example { public string Text = \"hello\"; public int Number = 42; }",
                &[
                    ("// 中文注释", "comment"),
                    ("class", "keyword"),
                    ("Example", "type"),
                    ("\"hello\"", "string"),
                    ("42", "number"),
                ],
            ),
            (
                "CMakeLists.txt",
                "# 中文注释\nset(VALUE \"hello\")\nif(VALUE)\nendif()\nset(COUNT 42)\n",
                &[
                    ("# 中文注释", "comment"),
                    ("if", "keyword"),
                    ("\"hello\"", "string"),
                    ("42", "number"),
                ],
            ),
            (
                "file.tsx",
                "const value: number = 42; const view = <div title=\"中文\">hello</div>;",
                &[
                    ("const", "keyword"),
                    ("number", "type"),
                    ("42", "number"),
                    ("div", "tag"),
                    ("title", "attribute"),
                    ("\"中文\"", "string"),
                ],
            ),
            (
                "file.jsx",
                "const view = <div title=\"中文\">hello</div>;",
                &[
                    ("const", "keyword"),
                    ("div", "tag"),
                    ("title", "attribute"),
                    ("\"中文\"", "string"),
                ],
            ),
        ];
        for (path, code, tokens) in samples {
            let mut highlighter = SyntaxHighlighter::new(language_for_source(path, code));
            assert!(highlighter.update(None, &Rope::from_str(code), None));
            for theme in [
                HighlightTheme::default_dark(),
                HighlightTheme::default_light(),
            ] {
                let styles = highlighter.styles(&(0..code.len()), theme.as_ref());
                for (token, category) in *tokens {
                    let offset = code.find(token).unwrap();
                    let actual = styles
                        .iter()
                        .find(|(range, _)| range.contains(&offset))
                        .and_then(|(_, style)| style.color);
                    let expected = theme
                        .style
                        .syntax
                        .style(category)
                        .and_then(|style| style.color);
                    assert!(expected.is_some(), "{path}: theme has no {category} color");
                    assert_eq!(
                        actual, expected,
                        "{path}: {token:?} should be {category} in {}",
                        theme.name
                    );
                }
            }
        }
    }
}
