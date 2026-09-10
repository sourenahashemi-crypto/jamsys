export default {
    ActorAlign: {CENTER: 1},
    BinLayout: class {},
    EVENT_STOP: true,
    EVENT_PROPAGATE: false,
    // Real values from Clutter. SMOOTH is what Wayland actually delivers, so a
    // stub without it would let a handler that ignores smooth scroll pass.
    ScrollDirection: {UP: 0, DOWN: 1, LEFT: 2, RIGHT: 3, SMOOTH: 4},
};
