<project-context>
	yuka is a Wayland compositor and window manager: an infinite canvas where
	windows are quads we can smoothly zoom, pan, and move, drawn through
	shaders the scripts choose per window.

	Language: Rust for the engine, LuaJIT (via mlua, vendored) for behaviour.
	Renderer: raylib (via FFI / raylib-rs). Wayland protocol frontend: Smithay.

	The vision: Rust is only the performant part, and Lua defines the user
	experience. Changing how yuka feels or looks should not need a Rust
	change.

	Rust is the engine: the Wayland protocol, buffer import and drawing,
	gesture recognition from raw touches, the mechanics of following the
	pointer and of the resize handshake, session and input. Lua is
	everything that decides: key bindings, what gestures mean, where
	scroll and pinch go and how scroll feels to clients, camera motion,
	window placement and focus, maximize, every animation, how each window
	is drawn (opacity, lift, shadow, stretch, shader), the cursor over the
	canvas, and the debug overlays. The scripts live in lua/, entry
	lua/init.lua; window shaders live in shaders/. lua/config.lua holds
	only the settings the engine reads.

	yuka does not start without scripts that load. A reload that fails
	keeps the running scripts and shows the error on screen. Quit and VT
	switching stay in Rust so a broken script can never lock the machine,
	and so does the error banner, since it has to work when Lua is broken.

	The codebase has three zones with different rules:

	1. The protocol frontend (Smithay facing). A small, contained set of
	   modules that implement Smithay's Handler traits and hold its reference
	   counted surface handles. Trait impls and Arc/Rc are unavoidable here.
	   Keep this zone as small as possible.

	2. The script boundary (src/lua.rs). The only module that touches mlua
	   types. mlua's traits and handles stay inside it; what crosses out is
	   plain data. Each frame the engine fills a snapshot table and calls
	   yk.frame once. Scripts read the snapshot and call yk.* functions, which
	   queue commands (lua::Cmd). The main loop drains them in order after the
	   call returns, so the engine stays the only writer of its state. Scripts
	   name windows by render::Windows ids, never by surface. To give scripts
	   a new power, add a Cmd variant (or a snapshot field) and its yk.*
	   function here, and apply it in the main loop. Engine data that is
	   only needed sometimes (the overlays' yk.surfaces and yk.pad) is
	   gathered only on frames the scripts ask for it.

	3. Everything else (canvas, camera, window management, input mapping,
	   shader loading, rendering). This is the code we hand craft. It follows
	   the Data Oriented rules below with no exceptions.
</project-context>

<instructions-for-writing-code>
	Use snake_case for variable and function names.
	Use snake_case for module names.
	Use UpperCamelCase for types (structs, enums, traits).
	Use UPPER_SNAKE_CASE for constants and statics.

	Write plain Rust. Avoid clever abstraction. Prefer free functions that take
	data and transform it over methods that hide state.

	Do not build OOP in Rust. No trait objects (dyn) used as vtables, no
	"manager" structs that own everything and expose behavior through methods,
	no builder or visitor patterns where a plain function would do. Traits are
	allowed only where an external API demands them (Smithay Handler impls at
	the protocol boundary). Do not invent traits for our own code just to feel
	generic. Prefer concrete types.

	Prefer Data Oriented Design. Group data by how it is accessed and
	transformed rather than by "object" identity. Prefer structs of arrays
	(struct of Vec) over arrays of structs (Vec of struct) when iteration
	patterns benefit from it.

	Reference long lived data by index or handle, not by pointer or reference.
	The borrow checker fights long lived references anyway, and index handles
	are the Data Oriented choice. Use a generation counter to detect stale
	handles.

	Prefer arena allocation over many small allocations:

		Per frame scratch: a bump arena (bumpalo) or a reused Vec cleared once
		per frame. Reset in bulk, never free individual items.

		Long lived heterogeneous data: a Vec plus index handles (a slotmap or
		generational arena). This is arena allocation. Allocate in bulk, drop
		the whole Vec when the lifetime ends.

	Avoid Rc and Arc for data we own. They are per item reference counting
	dressed up. They are acceptable only where Smithay hands them to us.

	Prefer stack allocation and fixed size arrays when the lifetime and size
	are known. Prefer Vec::with_capacity when the count is known up front so we
	allocate once.

	Prefer explicit sizing with usize and slices over iterator chains when the
	slice form is clearer. Keep hot loops as plain indexed loops when that is
	easier to read and reason about.

	Keep unsafe contained. All FFI to raylib and any raw pointer work lives
	behind a thin, clearly named wrapper. At the boundary convert C types into
	safe Rust types immediately (slices with known length, usize, owned
	values). Never let raw pointers or *const c_char leak into the Data
	Oriented zone.

	For comments:

	use heading-type comments like:

	//
	// Constants
	//

	const MAX_WINDOWS: usize = 1024;

	//
	// Types
	//

	struct Positions {
	    x: Vec<f32>,
	    y: Vec<f32>,
	    vx: Vec<f32>,
	    vy: Vec<f32>,
	    count: u32,
	}

	//
	// State
	//

	// State is owned in one struct and threaded explicitly by &mut.
	// Do not reach for global mutable statics.

	struct World {
	    positions: Positions,
	}

	//
	// Functions
	//

	fn update_positions(p: &mut Positions, dt: f32) {
	    for i in 0..p.count as usize {
	        p.x[i] += p.vx[i] * dt;
	        p.y[i] += p.vy[i] * dt;
	    }
	}

	//
	// Entry
	//

	fn main() {
	    // ...
	}

	you get the idea.

	Index handle pattern for long lived data:

	//
	// Types
	//

	#[derive(Clone, Copy, PartialEq)]
	struct WindowId {
	    index: u32,
	    generation: u32,
	}

	struct Windows {
	    transform: Vec<[f32; 16]>,
	    texture: Vec<u32>,
	    generation: Vec<u32>,
	    count: usize,
	}

	//
	// Functions
	//

	fn windows_get(w: &Windows, id: WindowId) -> Option<usize> {
	    let i = id.index as usize;
	    if i < w.count && w.generation[i] == id.generation {
	        Some(i)
	    } else {
	        None
	    }
	}

	For inline comments keep them minimal and precise. Avoid using em dashes
	or dashes for punctuation.

</instructions-for-writing-code>

<instructions-for-writing-lua>
	The Lua in lua/ follows the same spirit as the Rust: plain functions
	over data, no classes or metatables used as objects, module tables
	returned from each file.

	Use the same heading comments, with -- instead of //:

	--
	-- Tuning
	--

	Behaviour knobs live in a tune table at the top of the module that
	uses them, each with a comment saying what it does and what the
	values feel like. lua/config.lua is only for settings the engine
	reads.

	Read the frame snapshot, write through yk.* commands. Never hold on
	to engine state across frames except by window id. A script that
	needs per window state keeps a table keyed by id and forgets ids
	that stop appearing in the snapshot.

	yk.frame runs every frame, so avoid allocating tables in it: reuse
	module level scratch tables and clear them. Allocating is fine in
	code that runs on an event (a key press, a window opening) or in
	the debug overlays.

	Order inside yk.frame matters where two modules touch the same
	state; init.lua says why things run in the order they do.
</instructions-for-writing-lua>

<instructions-for-answer-format>
	How to answer in general:
		Never use markdown.
		Use plain text like man pages.
		When showing code use:

		```[language]
		code example here
		```
</instructions-for-answer-format>

<instructions-for-git>
	Never add Co-Authored-By lines (or any other Claude attribution) to
	commit messages or pull request descriptions.
</instructions-for-git>
