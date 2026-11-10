//! Custom line icons for the chrome (toolbar, menus, nav, panel toggles), drawn
//! as authored SVG so they carry smooth curves and a consistent thin, rounded
//! stroke — one family, native to clew, in place of a third-party icon font.
//!
//! Each icon is original geometry on a 24-unit grid, `fill:none`, stroke width 2,
//! round caps and joins. The stroke color is injected per call (FG / DIM / …),
//! and the SVG is rendered by iced's `svg` widget (the same resvg backend used
//! for math/mermaid).

use iced::widget::svg;
use iced::{Color, Element};

/// Which icon to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    // Toolbar
    /// An open book: a page read as rendered.
    OpenBook,
    /// A page laid out in panels: the project's overview.
    Layout,
    Stats,
    Ask,
    Debug,
    CallGraph,
    ImportGraph,
    /// The type map: a base type over two derived ones.
    TypeMap,
    /// The project's maps: a table linked to two others.
    ProjectMaps,
    TextLines,
    Plug,
    Help,
    Settings,
    // More menu
    Note,
    Info,
    Lightbulb,
    Minimap,
    Sparkle,
    /// Play, and a spark: a run the AI works out (Fake Run).
    FakeRun,
    /// A warning triangle: the project's weak spots.
    WeakSpots,
    /// A step over, and a spark: a run the AI works out, stepped through
    /// (Fake Debug).
    FakeDebug,
    Compass,
    Skim,
    Folder,
    /// A folder, open: one to open.
    FolderOpen,
    Diff,
    TimeTravel,
    Remote,
    Shortcuts,
    // Chrome: nav + panel toggles
    ArrowLeft,
    ArrowRight,
    /// Back along the trail.
    ChevronLeft,
    PanelLeft,
    PanelRight,
    // Small UI actions / indicators
    Edit,
    Close,
    ChevronDown,
    CheckCircle,
    Circle,
    Search,
    Bookmark,
    // Graph-map header
    List,
    Cube,
    Pause,
    Play,
    Book,
    /// A prompt: a command to run (a lone chevron read as a submenu).
    Prompt,
    /// A book with an A on its cover: the glossary.
    Dictionary,
    /// A crosshair: put the view back where it starts.
    Recenter,
    Export,
    // Voice and the Ask window
    Mic,
    /// A speaker, sounding: Clew speaking.
    Speaker,
    /// A filled square: stop.
    Stop,
    Expand,
    Shrink,
    /// Two chevrons down: open every branch of a tree.
    ExpandAll,
    // The window's chrome
    /// The app menu of Windows and Linux (three bars).
    Menu,
    /// More (three dots).
    More,
    /// Narrow a list down (three bars, each shorter).
    Filter,
    /// Search the text of the files: lines of text under a lens.
    TextSearch,
    /// Find by meaning: a lens inside a frame's four corners.
    FindMeaning,
    /// The reading trail: two stops joined by a winding path.
    Route,
    /// A value's flow: a path that bends back on itself to an arrow.
    Flow,
    /// What the reader shows (the View menu).
    Eye,
    ChevronRight,
    /// Kept on this computer.
    Lock,
    /// Send what was typed (an arrow up).
    Send,
    Check,
    /// A file: a page with its corner folded.
    File,
    /// A pair of braces: a symbol, in the command bar.
    Braces,
    /// What leans on a function: one box over the boxes under it.
    Network,
    /// When something was: a clock face.
    Clock,
    /// A speech bubble: ask Clew, in the command bar.
    Message,
    // Settings' pages, and Ask's sessions
    Sun,
    Refresh,
    Plus,
}

/// The window buttons Clew draws on Windows and Linux. Their own family:
/// hairline geometry on a 10-unit grid, the way those systems draw them,
/// not the chrome's rounded strokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowGlyph {
    Minimize,
    Maximize,
    /// Maximized: two overlapping frames, "make it a window again".
    Restore,
    Close,
}

/// The inner SVG for a glyph — original geometry, 24×24, stroked (a few filled
/// dots set `fill`/`stroke` inline). Single-quoted attributes so the wrapper's
/// double-quoted `stroke` substitution stays clean.
fn body(g: Glyph) -> &'static str {
    use Glyph::*;
    match g {
        OpenBook => {
            "<path d='M2 3h6a4 4 0 0 1 4 4v14a3 3 0 0 0-3-3H2z'/>\
             <path d='M22 3h-6a4 4 0 0 0-4 4v14a3 3 0 0 1 3-3h7z'/>"
        }
        Layout => {
            "<rect x='3' y='3' width='18' height='18' rx='2'/><path d='M3 9h18'/><path d='M9 21V9'/>"
        }
        Stats => {
            "<path d='M3 3v18h18'/><path d='M18 17V9'/>\
             <path d='M13 17V5'/><path d='M8 17v-3'/>"
        }
        Ask => {
            "<path d='M6.5 5 H17.5 A2.5 2.5 0 0 1 20 7.5 V13.5 A2.5 2.5 0 0 1 17.5 16 H10 L6 19.5 V16 A2.5 2.5 0 0 1 3.5 13.5 V7.5 A2.5 2.5 0 0 1 6.5 5 Z'/>"
        }
        Debug => {
            "<path d='m8 2 1.88 1.88'/><path d='M14.12 3.88 16 2'/>\
             <path d='M9 7.13v-1a3.003 3.003 0 1 1 6 0v1'/>\
             <path d='M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6'/>\
             <path d='M12 20v-9'/><path d='M6.53 9C4.6 8.8 3 7.1 3 5'/><path d='M6 13H2'/>\
             <path d='M3 21c0-2.1 1.7-3.9 3.8-4'/><path d='M20.97 5c0 2.1-1.6 3.8-3.5 4'/>\
             <path d='M22 13h-4'/><path d='M17.2 17c2.1.1 3.8 1.9 3.8 4'/>"
        }
        CallGraph => {
            "<path d='M12 8.65 L7.3 15.25'/><path d='M12 8.65 L16.7 15.25'/>\
             <circle cx='12' cy='7.25' r='2.2'/><circle cx='6.5' cy='16.75' r='2.2'/><circle cx='17.5' cy='16.75' r='2.2'/>"
        }
        // One file bringing in another: two modules, the one joined to the
        // other (an arrow into a tray read as a download).
        ImportGraph => {
            "<rect x='3' y='3' width='8' height='7' rx='1.5'/>\
             <rect x='13' y='14' width='8' height='7' rx='1.5'/>\
             <path d='M7 10v5.5a2 2 0 0 0 2 2h4'/>"
        }
        TypeMap => {
            "<path d='M8.3 10a.7.7 0 0 1-.626-1.079L11.4 3a.7.7 0 0 1 1.198-.043L16.3 8.9a.7.7 0 0 1-.572 1.1Z'/>\
             <rect x='3' y='14' width='7' height='7' rx='1'/><circle cx='17.5' cy='17.5' r='3.5'/>"
        }
        // Sheets stacked: the project's graphs, one over another — schema,
        // routes, wiring. (A folded map is the Minimap's.)
        ProjectMaps => {
            "<path d='M12 3.5 L20.5 7.75 L12 12 L3.5 7.75 Z'/>\
             <path d='M3.5 12 L12 16.25 L20.5 12'/><path d='M3.5 16.25 L12 20.5 L20.5 16.25'/>"
        }
        // Lines of text and a spark: what clew writes of the code (lines
        // alone read as "align left").
        TextLines => {
            "<path d='M3 6h8'/><path d='M3 12h18'/><path d='M3 18h14'/>\
             <path d='M18 2.5C18.4 5 19 5.6 21.5 6C19 6.4 18.4 7 18 9.5C17.6 7 17 6.4 14.5 6C17 5.6 17.6 5 18 2.5Z'/>"
        }
        // A plug: what a language server is to clew.
        Plug => {
            "<path d='M12 22v-5'/><path d='M9 8V2'/><path d='M15 8V2'/>\
             <path d='M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z'/>"
        }
        // A question in a circle: help, the tutorial.
        Help => {
            "<circle cx='12' cy='12' r='10'/><path d='M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3'/>\
             <path d='M12 17h.01'/>"
        }
        Settings => {
            "<path d='M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z'/>\
             <circle cx='12' cy='12' r='3'/>"
        }
        Note => {
            "<rect x='5' y='3.5' width='14' height='17' rx='2'/>\
             <path d='M8.5 8 H15.5'/><path d='M8.5 11.5 H15.5'/><path d='M8.5 15 H13'/>"
        }
        Info => {
            "<circle cx='12' cy='12' r='8'/><path d='M12 11 V15.5'/>\
             <circle cx='12' cy='8.4' r='0.9' fill='STROKE' stroke='none'/>"
        }
        Lightbulb => {
            "<path d='M12 4 A5 5 0 0 1 15 13 C14.4 13.8 14 14.4 14 15.5 H10 C10 14.4 9.6 13.8 9 13 A5 5 0 0 1 12 4 Z'/>\
             <path d='M10 18 H14'/><path d='M10.7 20 H13.3'/>"
        }
        Minimap => {
            "<path d='M4 6.5 L9.5 4.5 L14.5 6.5 L20 4.5 V17.5 L14.5 19.5 L9.5 17.5 L4 19.5 Z'/>\
             <path d='M9.5 4.5 V17.5'/><path d='M14.5 6.5 V19.5'/>"
        }
        Sparkle => {
            "<path d='M12 4 C12.6 10 14 11.4 20 12 C14 12.6 12.6 14 12 20 C11.4 14 10 12.6 4 12 C10 11.4 11.4 10 12 4 Z'/>"
        }
        // A triangle with a mark in it: look here.
        WeakSpots => {
            "<path d='M10.3 4.2 L2.6 17.5 A2 2 0 0 0 4.3 20.5 H19.7 A2 2 0 0 0 21.4 17.5 L13.7 4.2 A2 2 0 0 0 10.3 4.2 Z'/>\
             <path d='M12 9.5 V13.5'/><circle cx='12' cy='16.8' r='0.9' fill='STROKE' stroke='none'/>"
        }
        // Play, and the spark of what the AI writes (as on TextLines).
        FakeRun => {
            "<path d='M5 7 L14 13 L5 19 Z'/>\
             <path d='M18 2.5C18.4 5 19 5.6 21.5 6C19 6.4 18.4 7 18 9.5C17.6 7 17 6.4 14.5 6C17 5.6 17.6 5 18 2.5Z'/>"
        }
        // A step over a line, and the spark.
        FakeDebug => {
            "<path d='M3.5 15.5 C4.5 9.5 13.5 9.5 14.5 15.5'/><path d='M11.6 14.6 L14.5 15.5 L15.5 12.6'/>\
             <circle cx='9' cy='19.5' r='1.4' fill='STROKE' stroke='none'/>\
             <path d='M18 2.5C18.4 5 19 5.6 21.5 6C19 6.4 18.4 7 18 9.5C17.6 7 17 6.4 14.5 6C17 5.6 17.6 5 18 2.5Z'/>"
        }
        Compass => "<circle cx='12' cy='12' r='8'/><path d='M12 6 L14.2 12 L12 18 L9.8 12 Z'/>",
        // Folded together: the bodies skimmed over.
        Skim => "<path d='m7 20 5-5 5 5'/><path d='m7 4 5 5 5-5'/>",
        Folder => {
            "<path d='M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z'/>"
        }
        FolderOpen => {
            "<path d='m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2'/>"
        }
        // Two versions, compared.
        Diff => {
            "<circle cx='18' cy='18' r='3'/><circle cx='6' cy='6' r='3'/>\
             <path d='M13 6h3a2 2 0 0 1 2 2v7'/><path d='M11 18H8a2 2 0 0 1-2-2V9'/>"
        }
        // A clock turned back.
        TimeTravel => {
            "<path d='M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8'/>\
             <path d='M3 3v5h5'/><path d='M12 7v5l4 2'/>"
        }
        Remote => {
            "<circle cx='12' cy='12' r='8'/><path d='M4 12 H20'/>\
             <path d='M12 4 C8.6 6 8.6 18 12 20 C15.4 18 15.4 6 12 4'/>"
        }
        Shortcuts => {
            "<path d='M10 8h.01'/><path d='M12 12h.01'/><path d='M14 8h.01'/><path d='M16 12h.01'/>\
             <path d='M18 8h.01'/><path d='M6 8h.01'/><path d='M7 16h10'/><path d='M8 12h.01'/>\
             <rect width='20' height='16' x='2' y='4' rx='2'/>"
        }
        ArrowLeft => "<path d='M11 5.5 L4.5 12 L11 18.5'/><path d='M5 12 H19.5'/>",
        ArrowRight => "<path d='M13 5.5 L19.5 12 L13 18.5'/><path d='M19 12 H4.5'/>",
        PanelLeft => "<rect width='18' height='18' x='3' y='3' rx='2'/><path d='M9 3v18'/>",
        PanelRight => "<rect width='18' height='18' x='3' y='3' rx='2'/><path d='M15 3v18'/>",
        Edit => "<path d='M15.5 5 L19 8.5 L8.5 19 L4.5 20 L5.5 16 Z'/><path d='M13.5 7 L17 10.5'/>",
        Close => "<path d='M6 6 L18 18'/><path d='M18 6 L6 18'/>",
        ChevronDown => "<path d='m6 9 6 6 6-6'/>",
        CheckCircle => "<circle cx='12' cy='12' r='8'/><path d='M8.2 12 L11 14.8 L15.8 9.4'/>",
        Circle => "<circle cx='12' cy='12' r='8'/>",
        // The design's own magnifier.
        Search => "<circle cx='11' cy='11' r='8'/><path d='m21 21-4.3-4.3'/>",
        Bookmark => "<path d='m19 21-7-4-7 4V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v16z'/>",
        List => {
            "<circle cx='5.5' cy='7' r='1' fill='STROKE' stroke='none'/><path d='M9 7 H19'/>\
             <circle cx='5.5' cy='12' r='1' fill='STROKE' stroke='none'/><path d='M9 12 H19'/>\
             <circle cx='5.5' cy='17' r='1' fill='STROKE' stroke='none'/><path d='M9 17 H19'/>"
        }
        Cube => {
            "<path d='M12 3.8 L19.5 8 V16 L12 20.2 L4.5 16 V8 Z'/>\
             <path d='M4.5 8 L12 12.2 L19.5 8'/><path d='M12 12.2 V20.2'/>"
        }
        Pause => {
            "<rect x='7.5' y='6' width='3' height='12' rx='1'/>\
             <rect x='13.5' y='6' width='3' height='12' rx='1'/>"
        }
        Play => "<path d='M8.5 5.5 L18 12 L8.5 18.5 Z'/>",
        Book => "<path d='M4 19.5v-15A2.5 2.5 0 0 1 6.5 2H20v20H6.5a2.5 2.5 0 0 1 0-5H20'/>",
        Prompt => "<path d='m5 7 5 5-5 5'/><path d='M12.5 17H19'/>",
        Dictionary => {
            "<path d='M4 19.5v-15A2.5 2.5 0 0 1 6.5 2H20v20H6.5a2.5 2.5 0 0 1 0-5H20'/>\
             <path d='m10.5 13 2.7-7 2.7 7'/><path d='M11.5 10.6h3.4'/>"
        }
        Recenter => {
            "<circle cx='12' cy='12' r='7'/><path d='M12 2v4'/><path d='M12 18v4'/>\
             <path d='M2 12h4'/><path d='M18 12h4'/>"
        }
        Export => {
            "<path d='M4 12v8a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-8'/>\
             <polyline points='16 6 12 2 8 6'/><line x1='12' x2='12' y1='2' y2='15'/>"
        }
        // A microphone on its stand.
        Mic => {
            "<rect x='9' y='3' width='6' height='11.5' rx='3'/>\
             <path d='M5.5 11 A6.5 6.5 0 0 0 18.5 11'/><path d='M12 17.5 V21'/>\
             <path d='M8.5 21 H15.5'/>"
        }
        // A speaker, and the sound it makes.
        Speaker => {
            "<path d='M11 5.5 L6.5 9.5 H3.5 V14.5 H6.5 L11 18.5 Z'/>\
             <path d='M15.2 9 A4 4 0 0 1 15.2 15'/><path d='M18 6.2 A8 8 0 0 1 18 17.8'/>"
        }
        // A filled square, its corners rounded: stop.
        Stop => "<rect x='6' y='6' width='12' height='12' rx='2.5' fill='STROKE' stroke='none'/>",
        // Two corners pulled outward: make larger.
        Expand => {
            "<path d='M14 4 H20 V10'/><path d='M20 4 L13.5 10.5'/>\
             <path d='M10 20 H4 V14'/><path d='M4 20 L10.5 13.5'/>"
        }
        // Two chevrons, one under the other: open every branch.
        ExpandAll => "<path d='m7 6 5 5 5-5'/><path d='m7 13 5 5 5-5'/>",
        // Two corners pushed inward: make smaller.
        Shrink => {
            "<path d='M20 10 H14 V4'/><path d='M14 10 L20.5 3.5'/>\
             <path d='M4 14 H10 V20'/><path d='M10 14 L3.5 20.5'/>"
        }
        Menu => "<path d='M4.5 7 H19.5'/><path d='M4.5 12 H19.5'/><path d='M4.5 17 H19.5'/>",
        More => {
            "<circle cx='12' cy='12' r='1'/><circle cx='19' cy='12' r='1'/>\
             <circle cx='5' cy='12' r='1'/>"
        }
        Filter => "<path d='M4 7 H20'/><path d='M7 12 H17'/><path d='M10 17 H14'/>",
        TextSearch => {
            "<path d='M21 6H3'/><path d='M10 12H3'/><path d='M10 18H3'/>\
             <circle cx='17' cy='15' r='3'/><path d='m21 19-1.9-1.9'/>"
        }
        FindMeaning => {
            "<path d='M3 7V5a2 2 0 0 1 2-2h2'/><path d='M17 3h2a2 2 0 0 1 2 2v2'/>\
             <path d='M21 17v2a2 2 0 0 1-2 2h-2'/><path d='M7 21H5a2 2 0 0 1-2-2v-2'/>\
             <circle cx='12' cy='12' r='3'/><path d='m16 16-1.9-1.9'/>"
        }
        Route => {
            "<circle cx='6' cy='19' r='3'/><circle cx='18' cy='5' r='3'/>\
             <path d='M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15'/>"
        }
        Flow => {
            "<path d='M4 7 H14.5 A3 3 0 0 1 14.5 13 H9.5 A3 3 0 0 0 9.5 19 H19.5'/>\
             <path d='M16.5 16 L19.5 19 L16.5 22'/>"
        }
        Eye => {
            "<path d='M2.8 12 C5 7.6 8.2 5.5 12 5.5 C15.8 5.5 19 7.6 21.2 12 C19 16.4 15.8 18.5 12 18.5 C8.2 18.5 5 16.4 2.8 12 Z'/>\
             <circle cx='12' cy='12' r='2.8'/>"
        }
        ChevronRight => "<path d='m9 18 6-6-6-6'/>",
        ChevronLeft => "<path d='m15 18-6-6 6-6'/>",
        Lock => {
            "<rect x='5' y='10.5' width='14' height='9.5' rx='2'/>\
             <path d='M8.5 10.5 V8 A3.5 3.5 0 0 1 15.5 8 V10.5'/>"
        }
        Send => "<path d='M12 19.5 V5'/><path d='M6 11 L12 5 L18 11'/>",
        Check => "<path d='M5 12.5 L10 17.5 L19.5 7'/>",
        File => {
            "<path d='M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z'/>\
             <path d='M14 2v4a2 2 0 0 0 2 2h4'/>"
        }
        Braces => {
            "<path d='M8 3H7a2 2 0 0 0-2 2v5a2 2 0 0 1-2 2 2 2 0 0 1 2 2v5c0 1.1.9 2 2 2h1'/>\
             <path d='M16 21h1a2 2 0 0 0 2-2v-5c0-1.1.9-2 2-2a2 2 0 0 1-2-2V5a2 2 0 0 0-2-2h-1'/>"
        }
        Message => "<path d='M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z'/>",
        Sun => {
            "<circle cx='12' cy='12' r='4'/><path d='M12 2v2'/><path d='M12 20v2'/>\
             <path d='m4.93 4.93 1.41 1.41'/><path d='m17.66 17.66 1.41 1.41'/><path d='M2 12h2'/>\
             <path d='M20 12h2'/><path d='m6.34 17.66-1.41 1.41'/><path d='m19.07 4.93-1.41 1.41'/>"
        }
        Refresh => {
            "<path d='M21 12a9 9 0 0 0-9-9 9.75 9.75 0 0 0-6.74 2.74L3 8'/><path d='M3 3v5h5'/>\
             <path d='M3 12a9 9 0 0 0 9 9 9.75 9.75 0 0 0 6.74-2.74L21 16'/><path d='M16 16h5v5'/>"
        }
        Plus => "<path d='M5 12h14'/><path d='M12 5v14'/>",
        Network => {
            "<rect x='16' y='16' width='6' height='6' rx='1'/><rect x='2' y='16' width='6' height='6' rx='1'/>\
             <rect x='9' y='2' width='6' height='6' rx='1'/>\
             <path d='M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3'/><path d='M12 12V8'/>"
        }
        Clock => "<circle cx='12' cy='12' r='10'/><path d='M12 6v6l4 2'/>",
    }
}

/// A window button's glyph, stroked in `color` at `size` logical px.
pub fn window_icon<'a, M: 'a>(glyph: WindowGlyph, color: Color, size: f32) -> Element<'a, M> {
    let body = match glyph {
        WindowGlyph::Minimize => "<path d='M0.5 5 H9.5'/>",
        WindowGlyph::Maximize => "<rect x='0.5' y='0.5' width='9' height='9' rx='1.2'/>",
        WindowGlyph::Restore => {
            "<rect x='0.5' y='2.5' width='7' height='7' rx='1'/>\
             <path d='M2.5 2.5 V1.5 A1 1 0 0 1 3.5 0.5 H8.5 A1 1 0 0 1 9.5 1.5 V6.5 A1 1 0 0 1 8.5 7.5 H7.5'/>"
        }
        WindowGlyph::Close => "<path d='M0.5 0.5 L9.5 9.5'/><path d='M9.5 0.5 L0.5 9.5'/>",
    };
    let doc = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10' fill='none' stroke='{}' \
         stroke-width='1' stroke-linecap='round' stroke-linejoin='round'>{body}</svg>",
        hex(color),
    );
    svg(svg::Handle::from_memory(doc.into_bytes()))
        .width(size)
        .height(size)
        .into()
}

/// `iced::Color` → `#rrggbb`.
fn hex(c: Color) -> String {
    let u = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", u(c.r), u(c.g), u(c.b))
}

/// A square icon widget: `glyph` stroked in `color` at `size` logical px.
pub fn icon<'a, M: 'a>(glyph: Glyph, color: Color, size: f32) -> Element<'a, M> {
    svg(handle(glyph, color)).width(size).height(size).into()
}

/// [`icon`] faded to `opacity`: a control that can do nothing now, drawn
/// so — `color` alone could not say it (the stroke's colour is set in the
/// image, and a button's disabled style never reached it).
pub fn icon_faded<'a, M: 'a>(
    glyph: Glyph,
    color: Color,
    size: f32,
    opacity: f32,
) -> Element<'a, M> {
    svg(handle(glyph, color))
        .width(size)
        .height(size)
        .opacity(opacity)
        .into()
}

/// `glyph` in `color`, as an image to draw: what [`icon`] shows.
pub fn handle(glyph: Glyph, color: Color) -> svg::Handle {
    let stroke = hex(color);
    let doc = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='{stroke}' \
         stroke-width='1.9' stroke-linecap='round' stroke-linejoin='round'>{}</svg>",
        body(glyph).replace("STROKE", &stroke),
    );
    svg::Handle::from_memory(doc.into_bytes())
}
