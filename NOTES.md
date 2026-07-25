# Notes

## Cleanups

### `prune_dead` shifts eleven parallel vectors per removal

`src/render.rs` — `prune_dead` removes a dead window with `Vec::remove(i)` on
each of the parallel columns. `Vec::remove` shifts every element after `i`, so
one dead window is eleven memmoves, and `n` dead windows in a frame is O(n·len)
on each column.

Order in the store is not carried by index. Draw order comes from the `order`
column (`draw_toplevels` sorts by `z` then `order`) and placement comes from
`place_x`, so nothing observes the slot ordering. `swap_remove` is correct here
and is already what `DmabufCache::evict` and `release_held_dmabuf` use.

The loop has to not advance `i` after a swap, since the swapped-in element is
unexamined:

```rust
let mut i = 0;
while i < windows.surface.len() {
    if windows.surface[i].is_alive() {
        i += 1;
        continue;
    }
    if windows.owns[i] && windows.tex_id[i] != 0 {
        ray::unload_texture(windows.tex_id[i]);
    }
    windows.surface.swap_remove(i);
    windows.tex_id.swap_remove(i);
    // ... remaining columns
    // no i += 1: re-test the element swapped into this slot
}
```

Irrelevant at four windows. Relevant when an infinite canvas means hundreds.

### `draw_toplevels` allocates and sorts a fresh index list every frame

`src/render.rs` — the draw pass builds `let mut idx: Vec<usize> = ...collect()`
and sorts it, once per frame, forever. That is one heap allocation per frame in
the hot path.

`Windows` already owns a reused `scratch` buffer for shm row repacking; this
wants the same treatment, a `draw_order: Vec<usize>` field cleared and refilled
rather than allocated. That makes `draw_toplevels` take `&mut Windows`, which is
fine, or the scratch can be threaded in from the caller if the draw pass should
stay read-only over the store.

Secondary: the sort is a full comparison sort over all windows every frame, but
`z` only changes for windows that are lifting or settling and `order` only
changes on `front`. A dirty flag set by `animate`, `front`, `raise` and `settle`
would skip the sort entirely on the common frame where nothing moved.
