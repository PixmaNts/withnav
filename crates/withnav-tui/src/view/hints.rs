use crate::view::Hints;

pub const ARCHIVE: &[Hints] = &[Hints::new(
    &["alt+w close archive", "drag copy text", "page↑/↓ scroll"],
    1,
)];
pub const WATCHDOG_PAUSED: &[Hints] = &[Hints::new(&["alt+c continue", "esc stop"], 1)];
pub const CHILD_PAUSED: &[Hints] = &[Hints::new(
    &[
        "alt+c continue",
        "alt+w parent",
        "alt+s subagents",
        "esc stop",
    ],
    1,
)];
pub const CHILD_TRANSCRIPT: &[Hints] = &[Hints::new(
    &[
        "alt+w parent",
        "drag copy text",
        "alt+s subagents",
        "page↑/↓ scroll",
        "esc stop",
        "f1 help",
    ],
    0,
)];
pub const COPY_MODE: &[Hints] = &[Hints::new(
    &[
        "drag copy text",
        "shift+drag native",
        "shift+tab write",
        "f1 help",
    ],
    1,
)];
pub const SUBAGENTS: &[Hints] = &[Hints::new(
    &[
        "alt+s subagents",
        "tab build/plan",
        "drag copy text",
        "ctrl+p palette",
        "alt+1…9 tabs",
        "esc stop",
        "f1 help",
    ],
    1,
)];
pub const QUESTION: &[Hints] = &[Hints::new(
    &["↑↓ move", "space select", "enter Send", "esc plain typing"],
    4,
)];
pub const QUESTION_CUSTOM: &[Hints] = &[Hints::new(&["enter Send", "esc plain typing"], 2)];
pub const REJECT_MESSAGE: &[Hints] = &[
    Hints::new(
        &["enter reject with this message", "esc reject without one"],
        2,
    ),
    Hints::new(&["enter reject", "esc no message"], 2),
    Hints::new(&["enter reject", "esc omit"], 2),
];
pub const PERMISSION_ONCE: &[Hints] = &[
    Hints::new(&["o once", "r reject", "esc stop", "ctrl+p palette"], 2),
    Hints::new(&["o once", "r reject", "esc stop"], 2),
    Hints::new(&["o", "r"], 2),
];
pub const PERMISSION: &[Hints] = &[
    Hints::new(
        &[
            "o once",
            "a always",
            "r reject",
            "esc stop",
            "ctrl+p palette",
        ],
        3,
    ),
    Hints::new(&["o once", "a always", "r reject", "esc stop"], 3),
    Hints::new(&["o", "a", "r"], 3),
];
pub const IDLE: &[Hints] = &[Hints::new(
    &[
        "tab build/plan",
        "ctrl+p palette",
        "alt+1…9 tabs",
        "/model",
        "/compact",
        "esc stop",
        "alt+r panel",
        "f1 help",
    ],
    0,
)];
pub const TYPING: &[Hints] = &[Hints::new(
    &[
        "enter send",
        "alt+enter newline",
        "ctrl+p palette",
        "esc stop",
        "f1 help",
    ],
    0,
)];
