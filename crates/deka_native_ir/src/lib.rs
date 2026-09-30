//! Experimental native UI contract. No compiler, windowing, or evaluator dependency.
//! `program` adds the development wire format; production needs only owned UI values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub enum Length {
    #[default]
    Auto,
    Px(f32),
    Percent(f32),
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}
impl Edges {
    pub fn all(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub enum Align {
    Start,
    Center,
    End,
    #[default]
    Stretch,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    Between,
    Around,
    Evenly,
}
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub struct Keyframe {
    pub property: u8,
    pub at: f32,
    pub value: f32,
}
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub struct Motion {
    pub stiffness: f32,
    pub damping: f32,
    pub delay_ms: f32,
    pub stagger_ms: f32,
    pub enter: u8,
    pub exit: u8,
    pub layout: bool,
    pub frames: Vec<Keyframe>,
    pub repeats: u32,
    pub alternate: bool,
}
impl Default for Motion {
    fn default() -> Self {
        Self {
            stiffness: 0.,
            damping: 20.,
            delay_ms: 0.,
            stagger_ms: 0.,
            enter: 0,
            exit: 0,
            layout: false,
            frames: vec![],
            repeats: 1,
            alternate: false,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub struct Style {
    pub row: bool,
    pub wrap: bool,
    pub grow: f32,
    pub shrink: f32,
    pub align: Align,
    pub align_self: Option<Align>,
    pub justify: Justify,
    pub padding: Edges,
    pub margin: Edges,
    pub gap_x: f32,
    pub gap_y: f32,
    pub background: Option<u32>,
    pub color: Option<u32>,
    pub font_size: Option<f32>,
    pub radius: f32,
    pub width: Length,
    pub height: Length,
    pub min_width: Length,
    pub min_height: Length,
    pub max_width: Length,
    pub max_height: Length,
    pub clip: bool,
    pub nowrap: Option<bool>,
    pub scale: f32,
    pub rotate: f32,
    pub motion: Motion,
    pub opacity: f32,
    pub translate_x: f32,
    pub translate_y: f32,
    /// Bitset: opacity=1, translation=2, size=4, colours=8.
    pub transition: u8,
    pub duration_ms: f32,
    /// 0 linear, 1 ease-out, 2 ease-in-out.
    pub easing: u8,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            row: false,
            wrap: false,
            grow: 0.,
            shrink: 1.,
            align: Align::Stretch,
            align_self: None,
            justify: Justify::Start,
            padding: Edges::default(),
            margin: Edges::default(),
            gap_x: 0.,
            gap_y: 0.,
            background: None,
            color: None,
            font_size: None,
            radius: 0.,
            width: Length::Auto,
            height: Length::Auto,
            min_width: Length::Auto,
            min_height: Length::Auto,
            max_width: Length::Auto,
            max_height: Length::Auto,
            clip: false,
            nowrap: None,
            scale: 1.,
            rotate: 0.,
            motion: Motion::default(),
            opacity: 1.,
            translate_x: 0.,
            translate_y: 0.,
            transition: 0,
            duration_ms: 200.,
            easing: 1,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    pub style: Style,
    pub text: Option<String>,
    pub on_click: Option<usize>,
    pub children: Vec<Node>,
}
#[cfg(feature = "program")]
mod program {
    use super::Style;
    use serde::{Deserialize, Serialize};
    /// Bump when changing the development protocol. Readers must reject mismatches.
    pub const FORMAT_VERSION: u32 = 4;
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Program {
        pub format: u32,
        pub component: String,
        pub states: Vec<State>,
        pub root: Template,
        pub handlers: Vec<Update>,
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct State {
        pub name: String,
        pub initial: f64,
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub enum Condition {
        Equal(Number, Number),
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct StyleWhen {
        pub condition: Condition,
        pub then_style: Style,
        pub else_style: Style,
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Template {
        pub id: String,
        pub style: Style,
        pub style_when: Option<StyleWhen>,
        pub visible_when: Option<Condition>,
        pub text: Option<Text>,
        pub on_click: Option<usize>,
        pub children: Vec<Template>,
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub enum Number {
        Literal(f64),
        State(usize),
        Add(Box<Number>, Box<Number>),
        Sub(Box<Number>, Box<Number>),
        Mul(Box<Number>, Box<Number>),
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub enum Text {
        Literal(String),
        Number(Number),
    }
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    pub struct Update {
        pub state: usize,
        pub value: Number,
    }
}
#[cfg(feature = "program")]
pub use program::*;

mod style;
/// Apply the utility subset shared by compilation and live native components.
pub use style::{apply as apply_classes, for_element as element_style};

mod motion;
