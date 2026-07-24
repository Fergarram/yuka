//
// Wayland frontend zone
//
// This is the contained Smithay-facing zone. Trait impls and reference counted
// surface handles live here. Everything downstream (rendering, window store)
// stays in the Data Oriented zone.
//

pub mod state;
