use std::sync::LazyLock;

use regex::Regex;

// Modifiers that may stand before a declaration in Rust, TS/JS, Java, Kotlin, C++ and the like.
const MODIFIERS: &str = r"pub(?:\([^)]*\))?|export|default|declare|async|unsafe|static|abstract|public|private|protected|internal|extern|final|sealed|open|data|inline|override|suspend|companion|annotation|enum|value|inner|operator|infix|tailrec|virtual|constexpr|friend|const";

/// Declarations that start with a keyword: functions, types, classes, impls.
static KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^\s{{0,8}}(?:(?:{MODIFIERS})\s+)*(?:fn|struct|enum|trait|impl|mod|union|macro_rules!|function\*?|class|interface|@interface|record|object|namespace|template|def|func|fun)\b"
    ))
    .expect("valid regex")
});

/// Constants and type aliases, only at the top level.
static TOP_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"^(?:(?:{MODIFIERS})\s+)*(?:const|static|let|var|val|type|typealias|typedef)\s"
    ))
    .expect("valid regex")
});

/// Java, C++, C# and TS methods: a return type or modifier, then `name(`.
static SIGNATURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s{0,8}(?:[\w:<>,.\[\]*&?~]+\s+)+[*&]*~?[A-Za-z_][\w:]*\s*\(")
        .expect("valid regex")
});

/// C++ members defined outside their class: `Foo::bar(` or `Foo::~Foo(`.
static QUALIFIED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][\w<>]*::~?[A-Za-z_]\w*\s*\(").expect("valid regex"));

/// Words that start statements, not declarations, though they look like `type name(`.
const STATEMENTS: [&str; 31] = [
    "if",
    "else",
    "elif",
    "for",
    "foreach",
    "while",
    "switch",
    "match",
    "when",
    "return",
    "case",
    "new",
    "throw",
    "raise",
    "catch",
    "try",
    "do",
    "sizeof",
    "delete",
    "co_return",
    "co_await",
    "yield",
    "await",
    "defer",
    "go",
    "goto",
    "typeof",
    "not",
    "with",
    "assert",
    "using",
];

/// Declaration lines with their numbers, to pick ranges in a large file.
pub fn outline(lines: &[&str], cut: fn(&str) -> String) -> String {
    let found: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| declares(line))
        .map(|(i, line)| format!("{}\t{}", i + 1, cut(line.trim_end())))
        .collect();
    if found.is_empty() {
        return format!(
            "(no declarations recognized in {} lines; read the file instead)",
            lines.len()
        );
    }
    format!(
        "{}\n(outline: {} declarations in {} lines)",
        found.join("\n"),
        found.len(),
        lines.len()
    )
}

fn declares(line: &str) -> bool {
    let trimmed = line.trim_start();
    if ["//", "/*", "*", "#", "--"]
        .iter()
        .any(|c| trimmed.starts_with(c))
    {
        return false;
    }
    if KEYWORD.is_match(line) || TOP_LEVEL.is_match(line) || QUALIFIED.is_match(line) {
        return true;
    }
    // Only the part before `(` decides: default arguments may contain `=`.
    let head = line.split('(').next().unwrap_or("");
    let first = head
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| !w.is_empty())
        .unwrap_or("");
    SIGNATURE.is_match(line)
        && !STATEMENTS.contains(&first)
        && !head.contains("<<")
        && !head.contains('=')
}

#[cfg(test)]
mod tests {
    use super::declares;

    #[test]
    fn java() {
        for line in [
            "public class Main {",
            "    public static void main(String[] args) {",
            "    private final Map<String, List<Integer>> index(int size) throws IOException {",
            "    void run() {",
            "    public Main(int a) {",
            "public record Point(int x, int y) {}",
            "public @interface Marker {}",
            "    default String name() {",
            "    abstract int size();",
        ] {
            assert!(declares(line), "{line}");
        }
        for line in [
            "        if (a) {",
            "        return foo(a);",
            "        foo.bar(a);",
            "        int x = foo(a);",
            "        throw new IllegalStateException(e);",
        ] {
            assert!(!declares(line), "{line}");
        }
    }

    #[test]
    fn kotlin() {
        for line in [
            "data class User(val name: String)",
            "sealed class Result",
            "object Registry {",
            "    fun load(id: Int = 0): User {",
            "    override suspend fun fetch() = api.get()",
            "    companion object {",
            "private const val LIMIT = 10",
            "typealias Ids = List<Int>",
            "enum class Color { RED }",
        ] {
            assert!(declares(line), "{line}");
        }
        for line in [
            "        val x = load(1)",
            "        println(x)",
            "        when (x) {",
            "        return load(2)",
            " * Copyright (C) 2010-2025 The Team",
            "    // see class Foo (old)",
        ] {
            assert!(!declares(line), "{line}");
        }
    }

    #[test]
    fn cpp() {
        for line in [
            "namespace app {",
            "class Widget : public Base {",
            "struct Point { int x; };",
            "template <typename T>",
            "int main(int argc, char** argv) {",
            "static inline std::string name(const Foo& foo, int n = 0) {",
            "    virtual void draw() const override;",
            "void Widget::draw() const {",
            "Widget::Widget(int a) : a_(a) {}",
            "Widget::~Widget() {",
            "typedef unsigned int uint;",
            "enum class Mode { A, B };",
        ] {
            assert!(declares(line), "{line}");
        }
        for line in [
            "    std::cout << name(x) << std::endl;",
            "    auto w = make(1);",
            "    for (int i = 0; i < n; i++) {",
            "    delete ptr;",
            "    draw();",
        ] {
            assert!(!declares(line), "{line}");
        }
    }
}
