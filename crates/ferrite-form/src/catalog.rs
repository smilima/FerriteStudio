//! The Tool Palette catalog: what each control is in Slint terms.

/// The Slint type of an editable property, which also decides how the
/// Object Inspector edits it and how its value is written to `.slint`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropType {
    /// Free text, written as a quoted Slint string.
    String,
    /// A logical-pixel length, stored as a number and written as `12px`.
    Length,
    Bool,
    Int,
    Float,
    /// `#rrggbb` or `#rrggbbaa`; an empty value means "style default".
    Color,
    /// Comma-separated in the inspector, written as `["a", "b"]`.
    StringList,
}

impl PropType {
    pub fn slint_type(self) -> &'static str {
        match self {
            PropType::String => "string",
            PropType::Length => "length",
            PropType::Bool => "bool",
            PropType::Int => "int",
            PropType::Float => "float",
            PropType::Color => "color",
            PropType::StringList => "[string]",
        }
    }

    /// The Rust type Slint generates for this Slint type.
    pub fn rust_type(self) -> &'static str {
        match self {
            PropType::String => "slint::SharedString",
            PropType::Length | PropType::Float => "f32",
            PropType::Bool => "bool",
            PropType::Int => "i32",
            PropType::Color => "slint::Color",
            PropType::StringList => "slint::ModelRc<slint::SharedString>",
        }
    }

    /// Short label shown in the Object Inspector's type column.
    pub fn label(self) -> &'static str {
        match self {
            PropType::String => "string",
            PropType::Length => "length",
            PropType::Bool => "bool",
            PropType::Int => "int",
            PropType::Float => "float",
            PropType::Color => "color",
            PropType::StringList => "list",
        }
    }
}

/// An editable property. `default` is Slint's own default in inspector
/// form; a property whose value equals it is not written to the file.
#[derive(Debug)]
pub struct PropSpec {
    pub name: &'static str,
    pub ty: PropType,
    pub default: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: &'static str,
    pub ty: PropType,
}

/// A callback the control exposes, listed on the Events tab.
#[derive(Debug)]
pub struct CallbackSpec {
    pub name: &'static str,
    pub args: &'static [ArgSpec],
}

/// One entry of the Tool Palette.
#[derive(Debug)]
pub struct KindSpec {
    /// Slint element name, e.g. `Button` or `Rectangle`.
    pub kind: &'static str,
    /// True when the element must be imported from `std-widgets.slint`.
    pub std_widget: bool,
    /// Prefix for generated names: `button1`, `edit2`, ...
    pub prefix: &'static str,
    pub default_size: (i32, i32),
    pub props: &'static [PropSpec],
    pub callbacks: &'static [CallbackSpec],
    /// Property aliased on the form so Rust code can read and write it,
    /// the equivalent of `Edit1->Text` in C++Builder.
    pub published: Option<(&'static str, PropType)>,
    /// Values set when the control is dropped from the palette.
    /// `{Name}` is replaced with the capitalised control name.
    pub initial: &'static [(&'static str, &'static str)],
    /// One-line hint shown in the palette.
    pub hint: &'static str,
}

impl KindSpec {
    pub fn prop(&self, name: &str) -> Option<&'static PropSpec> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn callback(&self, name: &str) -> Option<&'static CallbackSpec> {
        self.callbacks.iter().find(|c| c.name == name)
    }
}

const fn p(name: &'static str, ty: PropType, default: &'static str) -> PropSpec {
    PropSpec { name, ty, default }
}

const fn a(name: &'static str, ty: PropType) -> ArgSpec {
    ArgSpec { name, ty }
}

use PropType::*;

/// Every control the palette offers, in palette order.
pub static KINDS: &[KindSpec] = &[
    KindSpec {
        kind: "Button",
        std_widget: true,
        prefix: "button",
        default_size: (96, 32),
        props: &[
            p("text", String, ""),
            p("enabled", Bool, "true"),
            p("primary", Bool, "false"),
            p("checkable", Bool, "false"),
        ],
        callbacks: &[CallbackSpec { name: "clicked", args: &[] }],
        published: Some(("text", String)),
        initial: &[("text", "{Name}")],
        hint: "Push button",
    },
    KindSpec {
        kind: "Text",
        std_widget: false,
        prefix: "label",
        default_size: (96, 24),
        props: &[
            p("text", String, ""),
            p("font-size", Length, "0"),
            p("font-weight", Int, "0"),
            p("color", Color, ""),
        ],
        callbacks: &[],
        published: Some(("text", String)),
        initial: &[("text", "{Name}")],
        hint: "Static label",
    },
    KindSpec {
        kind: "LineEdit",
        std_widget: true,
        prefix: "edit",
        default_size: (160, 32),
        props: &[
            p("text", String, ""),
            p("placeholder-text", String, ""),
            p("enabled", Bool, "true"),
            p("read-only", Bool, "false"),
        ],
        callbacks: &[
            CallbackSpec { name: "edited", args: &[a("text", String)] },
            CallbackSpec { name: "accepted", args: &[a("text", String)] },
        ],
        published: Some(("text", String)),
        initial: &[],
        hint: "Single-line text field",
    },
    KindSpec {
        kind: "TextEdit",
        std_widget: true,
        prefix: "memo",
        default_size: (200, 96),
        props: &[
            p("text", String, ""),
            p("placeholder-text", String, ""),
            p("enabled", Bool, "true"),
            p("read-only", Bool, "false"),
        ],
        callbacks: &[CallbackSpec { name: "edited", args: &[a("text", String)] }],
        published: Some(("text", String)),
        initial: &[],
        hint: "Multi-line text, like TMemo",
    },
    KindSpec {
        kind: "CheckBox",
        std_widget: true,
        prefix: "check",
        default_size: (120, 28),
        props: &[
            p("text", String, ""),
            p("checked", Bool, "false"),
            p("enabled", Bool, "true"),
        ],
        callbacks: &[CallbackSpec { name: "toggled", args: &[] }],
        published: Some(("checked", Bool)),
        initial: &[("text", "{Name}")],
        hint: "Check box",
    },
    KindSpec {
        kind: "Switch",
        std_widget: true,
        prefix: "switch",
        default_size: (120, 28),
        props: &[
            p("text", String, ""),
            p("checked", Bool, "false"),
            p("enabled", Bool, "true"),
        ],
        callbacks: &[CallbackSpec { name: "toggled", args: &[] }],
        published: Some(("checked", Bool)),
        initial: &[("text", "{Name}")],
        hint: "On/off switch",
    },
    KindSpec {
        kind: "ComboBox",
        std_widget: true,
        prefix: "combo",
        default_size: (160, 32),
        props: &[
            p("model", StringList, ""),
            p("current-index", Int, "0"),
            p("enabled", Bool, "true"),
        ],
        callbacks: &[CallbackSpec { name: "selected", args: &[a("value", String)] }],
        published: Some(("current-value", String)),
        initial: &[("model", "Item 1, Item 2, Item 3")],
        hint: "Drop-down list",
    },
    KindSpec {
        kind: "SpinBox",
        std_widget: true,
        prefix: "spin",
        default_size: (120, 32),
        props: &[
            p("value", Int, "0"),
            p("minimum", Int, "0"),
            p("maximum", Int, "100"),
            p("enabled", Bool, "true"),
        ],
        callbacks: &[CallbackSpec { name: "edited", args: &[a("value", Int)] }],
        published: Some(("value", Int)),
        initial: &[],
        hint: "Integer up/down",
    },
    KindSpec {
        kind: "Slider",
        std_widget: true,
        prefix: "slider",
        default_size: (160, 32),
        props: &[
            p("value", Float, "0"),
            p("minimum", Float, "0"),
            p("maximum", Float, "100"),
            p("enabled", Bool, "true"),
        ],
        callbacks: &[CallbackSpec { name: "changed", args: &[a("value", Float)] }],
        published: Some(("value", Float)),
        initial: &[],
        hint: "Track bar",
    },
    KindSpec {
        kind: "ProgressIndicator",
        std_widget: true,
        prefix: "progress",
        default_size: (160, 16),
        props: &[p("progress", Float, "0"), p("indeterminate", Bool, "false")],
        callbacks: &[],
        published: Some(("progress", Float)),
        initial: &[("progress", "0.4")],
        hint: "Progress bar, 0 to 1",
    },
    KindSpec {
        kind: "Rectangle",
        std_widget: false,
        prefix: "panel",
        default_size: (160, 96),
        props: &[
            p("background", Color, ""),
            p("border-radius", Length, "0"),
            p("border-width", Length, "0"),
            p("border-color", Color, ""),
        ],
        callbacks: &[],
        published: None,
        initial: &[("background", "#c8d3de"), ("border-radius", "4")],
        hint: "Coloured panel",
    },
];

/// Looks up a palette entry by Slint element name.
pub fn kind_spec(kind: &str) -> Option<&'static KindSpec> {
    KINDS.iter().find(|k| k.kind == kind)
}
