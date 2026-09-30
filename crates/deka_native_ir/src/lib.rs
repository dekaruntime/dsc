//! Experimental native UI contract. No compiler, windowing, or evaluator dependency.
//! `program` adds the development wire format; production needs only owned UI values.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "program", derive(serde::Serialize, serde::Deserialize))]
pub struct Style {
    pub row: bool,
    pub padding: f32,
    pub gap: f32,
    pub background: Option<u32>,
    pub color: Option<u32>,
    pub font_size: Option<f32>,
    pub radius: f32,
    pub width: Option<f32>,
    pub height: Option<f32>,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            row: false,
            padding: 0.,
            gap: 0.,
            background: None,
            color: None,
            font_size: None,
            radius: 0.,
            width: None,
            height: None,
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
    pub const FORMAT_VERSION: u32 = 1;
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
    pub struct Template {
        pub id: String,
        pub style: Style,
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
