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
        PLUGIN_STOPS,
        STEP_VERBS,
        MAP_WORDS,
    ]
    .concat()
}
/// clew-voice's models: each one's name and what it is for.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const VOICE_MODELS: &[&str] = &[
    tr_noop!("Tiny (English)"),
    tr_noop!("Fastest. Fine for short commands on a slow computer."),
    tr_noop!("Base (English)"),
    tr_noop!("Quick and accurate for commands and short questions in English."),
    tr_noop!("Small"),
    tr_noop!("Fastest multilingual model, for lower-memory computers."),
    tr_noop!("Large Turbo"),
    tr_noop!("Best balance of accuracy and speed, in any language."),
    tr_noop!("Large"),
    tr_noop!("Highest accuracy, for powerful computers."),
];

/// clew-voice's speech models (`speech::VOICE_MODELS`): each one's name and
/// what it is for.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SPEECH_MODELS: &[&str] = &[
    tr_noop!("Supertonic 3"),
    tr_noop!(
        "A small download for 31 languages, including English, French, Korean, Japanese, Dutch and Arabic. Chinese is read by Kokoro Chinese."
    ),
    tr_noop!("Kokoro Chinese"),
    tr_noop!(
        "Natural Chinese speech, with the English words in it read as English, in ten voices. Its first words play within a second or two."
    ),
    // The wake phrase's model (`wake::MODEL`).
    tr_noop!("Keyword Spotter"),
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
        "each file and an arrow to each file it includes. Its detail counts every file it pulls in, directly or indirectly, and every file that includes it pulls those in too"
    ),
    tr_noop!(
        "each method that overrides a virtual method of a base class, with an arrow to the one it overrides"
    ),
    tr_noop!(
        "each of the project's templates and the arguments it is instantiated with, an arrow from each instantiation to it"
    ),
    tr_noop!(
        "what each widget's build builds, nested as in the code, from runApp at the root down through the project's widgets (a State's build counts as its widget's) and the framework widgets under them. Each arrow is labelled with the slot it fills"
    ),
    tr_noop!(
        "where state lives and who reads it, for Riverpod providers (who watches, reads or listens to them), provider-package notifiers (who provides and reads them), and Blocs and Cubits (the events each handles, who adds them and who builds on them)"
    ),
    tr_noop!(
        "each supervisor and an arrow to each child its init/1 starts (labelled worker or supervisor), and an arrow from each application to the supervisor its start/2 starts"
    ),
    tr_noop!(
        "each message tag (a tuple's first atom), with an arrow from each module that sends it (!, gen_server:call or cast) and to each module that handles it (receive, handle_call, handle_cast, handle_info)"
    ),
    tr_noop!(
        "each function that calls or casts to a gen_server, an arrow to the handle_call or handle_cast clause that answers its request"
    ),
    tr_noop!(
        "each component the container creates, with an arrow to each dependency it is given (labelled with how), from an interface to its implementation, and from a configuration to the beans it provides"
    ),
    tr_noop!(
        "each model or entity the ORM maps, with an arrow to each one it relates to, labelled with the field and how (one-to-many, belongs to, …)"
    ),
    tr_noop!(
        "each action and filter, with an arrow to it from each file that fires it (do_action, apply_filters) and from it to each callback hooked on it (add_action, add_filter)"
    ),
    tr_noop!(
        "each type and the interfaces whose methods it has (Go code never states this), matched by method name and parameter count, including embedded interfaces and promoted methods"
    ),
    tr_noop!(
        "an arrow from a function to each goroutine it starts (go) and to each channel it sends on or closes, and from a channel to each function that receives from it"
    ),
    tr_noop!(
        "each of the project's packages and an arrow to each of the others it imports. The detail gives its layer (0 imports none of them) and any import cycle it is in"
    ),
    tr_noop!(
        "each type and an arrow to each class it is an instance of, from instance declarations, deriving clauses and standalone deriving"
    ),
    tr_noop!(
        "each monad stack that functions run in, read from their type signatures (its layers, outermost first, in the detail), and an arrow from each function to its stack"
    ),
    tr_noop!(
        "each component and an arrow to each of the project's components it renders, labelled with the props it passes"
    ),
    tr_noop!(
        "each store — Pinia, Redux, Zustand, Vuex, Svelte, Jotai, nanostores, MobX, NgRx, contexts — with the state and actions it holds, and an arrow from each component, hook or module that uses it, labelled with what it uses"
    ),
    tr_noop!(
        "each package of the monorepo (its version and exports in the detail) and an arrow to each of the others it depends on"
    ),
    tr_noop!(
        "each theorem and the project's theorems, definitions and structures its statement and proof use, resolved through namespaces and opens"
    ),
    tr_noop!(
        "each notation and the definition it stands for, and each simp lemma and the notation or definition its left-hand side rewrites"
    ),
    tr_noop!(
        "each class and the bases it names, in order, with its method resolution order (the order Python looks up methods in) in the detail"
    ),
    tr_noop!(
        "each fixture (its scope in its detail) and an arrow to it from each test or fixture that requests it by parameter name or with usefixtures"
    ),
    tr_noop!(
        "where Python picks what to call at run time (getattr with a computed name, dict registries of callables, functions a decorator registers) and the functions they can reach"
    ),
    tr_noop!(
        "each route, with its method and path, and an arrow to the handler it reaches (an inline handler is shown as the route itself)"
    ),
    tr_noop!(
        "the methods a class has without a def, and where they come from (modules mixed in, concerns' included blocks and class_methods, define_method, attr_*, delegate, alias, class_eval), plus calls made by name (send) and the classes that answer any call (method_missing)"
    ),
    tr_noop!("each type and an arrow to each trait it implements (impl Trait for Type) or derives"),
    tr_noop!(
        "each unsafe fn, unsafe impl, unsafe trait and unsafe block, with an arrow from each block to each of the project's unsafe functions it calls"
    ),
    tr_noop!(
        "each module and an arrow to each item it makes pub (not pub(crate)), and to what it re-exports with pub use"
    ),
    tr_noop!(
        "a card per table and view, as the migrations leave them, listing its columns, with a line from each foreign key's columns to the ones they reference"
    ),
    tr_noop!(
        "each migration in the order it runs, what it changes in its detail, and an arrow to each table it creates, alters or drops"
    ),
    tr_noop!(
        "an arrow from what is read to what is built from it, such as from a table to the view, CREATE TABLE … AS or INSERT … SELECT that reads it, through any CTEs in between"
    ),
    tr_noop!("an arrow from a source file to each table its SQL strings read or write"),
    tr_noop!(
        "each function with comptime parameters (a generic type constructor or a generic function) and an arrow to each instantiation of it in the project, placed where it first appears"
    ),
    tr_noop!(
        "each error set and the errors in it, and each function with the set its return type names, the errors it returns, the functions whose errors it passes on with “try”, and the errors it catches"
    ),
    tr_noop!(
        "each executable, library, test and module that build.zig builds, with its root source file, the libraries it links, the modules it imports, and the build steps that run it"
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
    tr_noop!("error set"),
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
        "This program's debug information comes from Zig's built-in code generator (the default for Debug builds on x86-64), and lldb can't read variables from it. Breakpoints and stepping work, but no variables show. Build it with -fllvm (or .use_llvm = true in build.zig) to see them."
    ),
    tr_noop!(
        "This is a Bun project, and Bun's debugger speaks WebKit's inspector protocol, which Clew's JavaScript debugger does not: the file is debugged under Node.js, where Bun's own APIs (Bun.serve, Bun.file) are missing. Run runs it with Bun."
    ),
    tr_noop!("larger than 64 MB"),
    tr_noop!("not a regular file inside the project (a link, or moved since the scan)"),
];
/// Why clew-plugin did not run, install or set up a plugin, said as it wrote
/// it: its approved file changed since (`clew_plugin::store::CHANGED`), and
/// the rest named below.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const PLUGIN_STOPS: &[&str] = &[
    tr_noop!(
        "Clew's copy of the plugin is missing or has changed since you approved it. Install the plugin again from its file, or ask the agent that made it to install it again."
    ),
    // Why a plugin was not installed (`clew_plugin::store::FILE_CHANGED`).
    tr_noop!("the plugin's file changed after it was read"),
    // Why a file picked to install is not a plugin
    // (`clew_plugin::inspect::NOT_A_COMPONENT` and the rest).
    tr_noop!("this is not a plugin (a plugin is a WebAssembly component built for wasm32-wasip2)"),
    tr_noop!("this WebAssembly component is not a Clew plugin because it has no plugin.toml"),
    tr_noop!("the plugin's file contains more than one plugin.toml"),
    tr_noop!("the plugin's manifest is not UTF-8 text"),
    tr_noop!(
        "the plugin's code does not export the interface Clew expects (build it with clew_plugin_sdk::export!)"
    ),
    // Why Clew stopped a run whose answer was too large
    // (`clew_plugin::runtime::ANSWER_TOO_LARGE`).
    tr_noop!("its answer was larger than Clew accepts from a plugin"),
    // A command refused an open outside the project
    // (`clew_plugin::runtime::OUTSIDE_PLACE`).
    tr_noop!("it asked to open a file outside the project"),
    // Why setting up Rust for plugins failed
    // (`clew_plugin::toolchain::NO_INSTALLER` and the rest).
    tr_noop!("rustup publishes no installer for this computer"),
    tr_noop!("the installer's published SHA-256 could not be read"),
    tr_noop!("the downloaded installer is not the one rust-lang.org published, so nothing was run"),
    // Rust being set up or removed in another Clew window
    // (`clew_plugin::toolchain::ELSEWHERE`).
    tr_noop!("another Clew window is setting up Rust or removing it"),
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

/// The English words a project graph's extractors (`clew_core::maps`) label
/// an edge or a node's detail with — code they quote (`pub use`, `select`,
/// `gen_server:call`) stays as written. The graph's list says them through
/// `ui::graph::map_text`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const MAP_WORDS: &[&str] = &[
    tr_noop!("inherits"),
    tr_noop!("instantiates"),
    tr_noop!("overrides"),
    tr_noop!("creates"),
    tr_noop!("sends"),
    tr_noop!("starts"),
    tr_noop!("constructor"),
    tr_noop!("field"),
    tr_noop!("fires"),
    tr_noop!("provides (@Bean)"),
    tr_noop!("resolves to"),
    tr_noop!("runs"),
    tr_noop!("closes"),
    tr_noop!("received by"),
    tr_noop!("runs in"),
    tr_noop!("means"),
    tr_noop!("rewrites"),
    tr_noop!("uses"),
    tr_noop!("handled by"),
    tr_noop!("includes"),
    tr_noop!("calls"),
    tr_noop!("reads"),
    tr_noop!("contains"),
    tr_noop!("depends on"),
    tr_noop!("installs"),
    tr_noop!("links"),
    tr_noop!("returns"),
    tr_noop!("root module"),
    tr_noop!("root"),
    tr_noop!("from"),
    tr_noop!("instance"),
    tr_noop!("application"),
    tr_noop!("supervisor"),
    tr_noop!("primary key"),
    tr_noop!("includes the includer"),
    tr_noop!("extends the includer"),
    tr_noop!("prepends the includer"),
    tr_noop!("handles every attribute the class lacks"),
    tr_noop!("answers calls to methods the class does not define"),
    tr_noop!("inferred error set"),
    tr_noop!("system library"),
    tr_noop!("no consistent method resolution order, so Python can't create this class"),
    tr_noop!("derives from"),
    tr_noop!("Pinia store"),
    tr_noop!("Redux slice"),
    tr_noop!("Redux store"),
    tr_noop!("Zustand store"),
    tr_noop!("Vuex store"),
    tr_noop!("Vuex module"),
    tr_noop!("Svelte store"),
    tr_noop!("Jotai atom"),
    tr_noop!("Nanostore"),
    tr_noop!("Context"),
    tr_noop!("MobX store"),
    tr_noop!("NgRx feature"),
    tr_noop!("NgRx signal store"),
];
