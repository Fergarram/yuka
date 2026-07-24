<instructions-for-writing-code>
	Use snake_case for variable names.
	Use snake_case for function names.
	Use UPPER_SNAKE_CASE for constants and macros.

	Never use C++ features. Write plain C (C11 or later).

	Never use OOP patterns. No function pointer tables pretending to be vtables,
	no "object" structs with self-referencing function pointers.

	Prefer Data Oriented Design when useful. Group data by how it is accessed
	and transformed rather than by "object" identity. Prefer structs of arrays
	over arrays of structs when iteration patterns benefit from it.

	Prefer arena allocations over many spread malloc/free calls. Allocate memory
	in bulk through an arena and free it all at once when the lifetime ends.
	Reserve individual malloc/free for cases where arena allocation genuinely
	does not fit.

	Prefer static/stack allocation when the lifetime and size are known.

	Prefer explicit sizing with size_t over null-terminated length discovery
	when practical.

	For comments:

	use heading-type comments like:

	//
	// Constants
	//

	#define MAX_ENTITIES 1024

	//
	// Types
	//

	typedef struct {
	    float* x;
	    float* y;
	    uint32_t count;
	} Positions;

	//
	// State
	//

	static Positions positions;

	//
	// Functions
	//

	static void update_positions(Positions* p, float dt) {
	    // ...
	}

	//
	// Entry
	//

	int main(void) {
	    // ...
	}

	you get the idea.

	For inline comments keep them minimal and precise. Avoid using em dashes
	or dashes for punctuation.

</instructions-for-writing-code>
```

mention that we prefer type* over *name

```md
<instructions-for-writing-code>
	Use snake_case for variable names.
	Use snake_case for function names.
	Use UPPER_SNAKE_CASE for constants and macros.

	Never use C++ features. Write plain C (C11 or later).

	Never use OOP patterns. No function pointer tables pretending to be vtables,
	no "object" structs with self-referencing function pointers.

	Prefer Data Oriented Design when useful. Group data by how it is accessed
	and transformed rather than by "object" identity. Prefer structs of arrays
	over arrays of structs when iteration patterns benefit from it.

	Prefer arena allocations over many spread malloc/free calls. Allocate memory
	in bulk through an arena and free it all at once when the lifetime ends.
	Reserve individual malloc/free for cases where arena allocation genuinely
	does not fit.

	Prefer static/stack allocation when the lifetime and size are known.

	Prefer explicit sizing with size_t over null-terminated length discovery
	when practical.

	Place the pointer asterisk with the type, not the name:

		float* x;

	not:

		float *x;

	For comments:

	use heading-type comments like:

	//
	// Constants
	//

	#define MAX_ENTITIES 1024

	//
	// Types
	//

	typedef struct {
	    float* x;
	    float* y;
	    uint32_t count;
	} Positions;

	//
	// State
	//

	static Positions positions;

	//
	// Functions
	//

	static void update_positions(Positions* p, float dt) {
	    // ...
	}

	//
	// Entry
	//

	int main(void) {
	    // ...
	}

	you get the idea.

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

