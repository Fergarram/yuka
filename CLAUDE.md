<project-context>
	om_wm is a Wayland compositor and window manager: an infinite canvas where
	windows are quads we can smoothly zoom, pan, and move, drawn through a
	shader pass that applies to all windows.

	Language: Rust. Renderer: raylib (via FFI / raylib-rs). Wayland protocol
	frontend: Smithay.

	The codebase has two zones with different rules:

	1. The protocol frontend (Smithay facing). A small, contained set of
	   modules that implement Smithay's Handler traits and hold its reference
	   counted surface handles. Trait impls and Arc/Rc are unavoidable here.
	   Keep this zone as small as possible.

	2. Everything else (canvas, camera, window management, input mapping,
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

<instructions-for-answer-format>
	How to answer in general:
		Never use markdown.
		Use plain text like man pages.
		When showing code use:

		```[language]
		code example here
		```
</instructions-for-answer-format>
