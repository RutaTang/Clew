//! English that other crates write and the app shows through
//! [`crate::i18n::lookup`]: listed here, as text the program shows, so every
//! catalog carries it. `i18n`'s tests check the list against those crates,
//! so a label they add or reword is not left untranslated unnoticed.

use crate::tr_noop;

/// Every text below, for the tests.
#[cfg(test)]
pub(crate) fn all() -> Vec<&'static str> {
    [
        VOICE_MODELS,
        SPEECH_MODELS,
        APPROVE,
        PROVIDERS,
        ENTRY_KINDS,
        MAP_TITLES,
        MAP_DESCRIPTIONS,
        MAP_KINDS,
        NOTICES,
        STEP_VERBS,
    ]
    .concat()
}
/// clew-voice's models: each one's name and what it is for.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const VOICE_MODELS: &[&str] = &[
    tr_noop!("Tiny (English)"),
    tr_noop!("Fastest; fine for short commands on a slow machine."),
    tr_noop!("Base (English)"),
    tr_noop!("Quick and accurate for commands and short questions in English."),
    tr_noop!("Small"),
    tr_noop!("Fastest multilingual model, for lower-memory computers."),
    tr_noop!("Medium (Large v3 Turbo)"),
    tr_noop!("Best balance of accuracy and speed, in any language."),
    tr_noop!("Large (Large v3)"),
    tr_noop!("Highest accuracy, for powerful computers."),
];

/// clew-voice's speech models (`speech::VOICE_MODELS`): each one's name and
/// what it is for.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SPEECH_MODELS: &[&str] = &[
    tr_noop!("Supertonic 3"),
    tr_noop!("Natural speech in 31 languages, English, Korean, Japanese and French among them."),
    tr_noop!("Kokoro (Chinese)"),
    tr_noop!("Mandarin Chinese, which Supertonic does not speak."),
];

/// When an MCP server's tool asks first (clew-core's `Approve::label`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const APPROVE: &[&str] = &[
    tr_noop!("Ask every time"),
    tr_noop!("Ask unless read-only"),
    tr_noop!("Never ask"),
];

/// The one AI provider that is not a brand name (clew-core's `Provider::label`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const PROVIDERS: &[&str] = &[tr_noop!("Custom (OpenAI-compatible)")];

/// What starts a program (clew-core's `EntryKind::label`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ENTRY_KINDS: &[&str] = &[
    tr_noop!("main"),
    tr_noop!("route"),
    tr_noop!("command"),
    tr_noop!("handler"),
];

/// Project maps' titles (clew-core's `maps`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const MAP_TITLES: &[&str] = &[
    tr_noop!("Include cost"),
    tr_noop!("Virtual overrides"),
    tr_noop!("Template instantiations"),
    tr_noop!("Widget tree"),
    tr_noop!("State management"),
    tr_noop!("Supervision tree"),
    tr_noop!("Messages"),
    tr_noop!("gen_server calls"),
    tr_noop!("Dependency injection"),
    tr_noop!("Entity relations"),
    tr_noop!("WordPress hooks"),
    tr_noop!("Interface satisfaction"),
    tr_noop!("Goroutines and channels"),
    tr_noop!("Go packages"),
    tr_noop!("Type class instances"),
    tr_noop!("Effect stacks"),
    tr_noop!("Component tree"),
    tr_noop!("Workspace packages"),
    tr_noop!("Theorem dependencies"),
    tr_noop!("Notation and simp"),
    tr_noop!("Class hierarchy (MRO)"),
    tr_noop!("pytest fixtures"),
    tr_noop!("Dynamic dispatch"),
    tr_noop!("Routes"),
    tr_noop!("Metaprogramming"),
    tr_noop!("Traits and derives"),
    tr_noop!("Unsafe code"),
    tr_noop!("Public API"),
    tr_noop!("Database schema"),
    tr_noop!("Schema history"),
    tr_noop!("Data lineage"),
    tr_noop!("Tables in code"),
    tr_noop!("Comptime generics"),
    tr_noop!("Error sets"),
    tr_noop!("Build graph"),
];

/// What each project map draws (clew-core's `maps`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const MAP_DESCRIPTIONS: &[&str] = &[
    tr_noop!(
        "each file and an arrow to each file it includes; its detail counts what it pulls in all told — every file that includes it pays for that"
    ),
    tr_noop!(
        "each method that overrides a virtual method of a base class, with an arrow to the one it overrides"
    ),
    tr_noop!(
        "each of the project's templates and the arguments it is instantiated with, an arrow from each instantiation to it"
    ),
    tr_noop!(
        "what each widget's build builds, nested as written: the project's widgets (a State's build is its widget's) and the framework widgets under them, each arrow labelled by the slot it fills; runApp at the root"
    ),
    tr_noop!(
        "where state lives and who reads it: Riverpod providers and who watches, reads or listens to them; provider-package notifiers, who provides and who reads them; Blocs and Cubits, the events each handles, who adds them and who builds on them"
    ),
    tr_noop!(
        "each supervisor and an arrow to each child its init/1 starts (labelled worker or supervisor); an application to the supervisor its start/2 starts"
    ),
    tr_noop!(
        "each message tag (a tuple's first atom); an arrow from each module that sends it — !, gen_server:call or cast — and to each module that takes it — receive, handle_call, handle_cast, handle_info"
    ),
    tr_noop!(
        "each function that calls or casts to a gen_server, an arrow to the handle_call or handle_cast clause that answers its request"
    ),
    tr_noop!(
        "each component the container makes; an arrow to what it is given (labelled how), from an interface to what it resolves to, from a configuration to the beans it provides"
    ),
    tr_noop!(
        "each model or entity the ORM maps; an arrow to each it relates to, labelled with the field and how (one-to-many, belongs to, …)"
    ),
    tr_noop!(
        "each action and filter; an arrow from each file that fires it (do_action, apply_filters) to it, and from it to each callback hooked on (add_action, add_filter)"
    ),
    tr_noop!(
        "each type and the interfaces whose methods it has — Go never says so in the code; matched by method name and parameter count, embedded interfaces and promoted methods included"
    ),
    tr_noop!(
        "an arrow from a function to each goroutine it starts (go), and to each channel it sends on or closes; from a channel to each function that receives from it"
    ),
    tr_noop!(
        "each of the project's packages and an arrow to each of the others it imports; the detail gives its layer (0 imports none of them) and any import cycle it is in"
    ),
    tr_noop!(
        "each type and an arrow to each class it is an instance of — instance declarations, deriving clauses and standalone deriving"
    ),
    tr_noop!(
        "each monad stack a function's type signature runs it in (its layers, outermost first, in the detail), and an arrow from each function to its stack"
    ),
    tr_noop!(
        "each component and an arrow to each of the project's components it renders, labelled with the props it passes"
    ),
    tr_noop!(
        "each package of the monorepo (its version and exports in the detail) and an arrow to each of the others it depends on"
    ),
    tr_noop!(
        "each theorem and the project's theorems, definitions and structures its statement and proof use, resolved through namespaces and opens"
    ),
    tr_noop!(
        "each notation and the definition it stands for; each simp lemma and the notation or definition its left-hand side rewrites"
    ),
    tr_noop!(
        "each class and the bases it names, in order; its detail is the method resolution order Python looks a method up in"
    ),
    tr_noop!(
        "each fixture (its scope in its detail) and an arrow to it from each test or fixture that asks for it — by parameter name, or with usefixtures"
    ),
    tr_noop!(
        "where Python picks what to call at run time: getattr with a computed name, dict registries of callables, functions a decorator registers — and the functions they can reach"
    ),
    tr_noop!(
        "each route — its method and path — with an arrow to the handler it reaches; a handler written in place is the route itself"
    ),
    tr_noop!(
        "the methods a class has that no def names, and where they come from: modules mixed in, concerns' included blocks and class_methods, define_method, attr_*, delegate, alias, class_eval; calls made by name (send) and the classes that answer any call (method_missing)"
    ),
    tr_noop!("each type and an arrow to each trait it implements (impl Trait for Type) or derives"),
    tr_noop!(
        "each unsafe fn, unsafe impl, unsafe trait and unsafe block; an arrow from a block to each of the project's unsafe functions it calls"
    ),
    tr_noop!(
        "each module and an arrow to each item it makes pub (not pub(crate)), and to what it re-exports with pub use"
    ),
    tr_noop!(
        "a node per table and view, as the migrations leave them; an arrow is a foreign key (labelled by its columns), or what a view reads"
    ),
    tr_noop!(
        "each migration in the order it runs, what it changes in its detail, and an arrow to each table it creates, alters or drops"
    ),
    tr_noop!(
        "an arrow from what is read to what is made of it: a table into the view, the CREATE TABLE … AS or the INSERT … SELECT that reads it, through the CTEs on the way"
    ),
    tr_noop!("an arrow from a source file to each table its SQL strings read or write"),
    tr_noop!(
        "each function with comptime parameters — a generic type constructor or a generic function — and an arrow to each instantiation the project makes of it, where it is first made"
    ),
    tr_noop!(
        "each error set and the errors in it; each function and the set its return type names, the errors it returns, the functions whose errors try passes on, and the errors it catches"
    ),
    tr_noop!(
        "what build.zig builds: each executable, library, test and module and its root source file, the libraries it links, the modules it imports, and the build steps that run it"
    ),
];

/// The kinds of a map's nodes, its list's headings; names of code (`cte`, `def`, `simp`) stay as they are.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const MAP_KINDS: &[&str] = &[
    tr_noop!("action"),
    tr_noop!("callback"),
    tr_noop!("channel"),
    tr_noop!("class"),
    tr_noop!("clause"),
    tr_noop!("component"),
    tr_noop!("entity"),
    tr_noop!("error"),
    tr_noop!("event"),
    tr_noop!("external"),
    tr_noop!("file"),
    tr_noop!("filter"),
    tr_noop!("fixture"),
    tr_noop!("function"),
    tr_noop!("generic"),
    tr_noop!("goroutine"),
    tr_noop!("handler"),
    tr_noop!("instance"),
    tr_noop!("instantiation"),
    tr_noop!("interface"),
    tr_noop!("message"),
    tr_noop!("method"),
    tr_noop!("migration"),
    tr_noop!("mixin"),
    tr_noop!("module"),
    tr_noop!("notation"),
    tr_noop!("package"),
    tr_noop!("process"),
    tr_noop!("provider"),
    tr_noop!("registry"),
    tr_noop!("re-export"),
    tr_noop!("route"),
    tr_noop!("service"),
    tr_noop!("set"),
    tr_noop!("stack"),
    tr_noop!("system"),
    tr_noop!("table"),
    tr_noop!("template"),
    tr_noop!("test"),
    tr_noop!("trait"),
    tr_noop!("type"),
    tr_noop!("view"),
    tr_noop!("widget"),
    tr_noop!("unsafe block"),
    tr_noop!("unsafe fn"),
    tr_noop!("unsafe impl"),
    tr_noop!("unsafe trait"),
    tr_noop!("std interface"),
];

/// Notices clew-core writes that the app shows as they are: why Zig's own debug information shows no variables, why a file was not searched.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const NOTICES: &[&str] = &[
    tr_noop!(
        "This program's debug information is Zig's own (from Zig's built-in code generator, the default for Debug builds on x86-64), and lldb reads no variables from it: breakpoints and stepping work, but no variables show. Build it with -fllvm (or .use_llvm = true in build.zig) to see them."
    ),
    tr_noop!("larger than 64 MB"),
    tr_noop!("not a regular file inside the project (a link, or moved since the scan)"),
];
/// The words an agent step's title starts with, as clew-server writes it
/// (`read src/lib.rs:1-50`), capitalized as the step shows them.
pub(crate) const STEP_VERBS: &[&str] = &[
    tr_noop!("Read"),
    tr_noop!("Outline"),
    tr_noop!("Search"),
    tr_noop!("Find"),
    tr_noop!("Files"),
    tr_noop!("History"),
    tr_noop!("Changes"),
    tr_noop!("Explanations"),
];
