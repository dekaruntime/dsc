use crate::{Align, Edges, Justify, Length, Style};

/// Classes are applied left to right; later declarations win. Unknown utilities fail.
pub fn apply(style: &mut Style, classes: &str) -> Result<(), String> {
    for class in classes.split_whitespace() {
        if crate::motion::apply(style, class)? {
            continue;
        }
        match class {
            "transition-none" => style.transition = 0,
            "transition-opacity" => style.transition = 1,
            "transition-transform" => style.transition = 2,
            "transition-size" => style.transition = 4,
            "transition-colors" => style.transition = 8,
            "transition-all" => style.transition = 15,
            "ease-linear" => style.easing = 0,
            "ease-out" => style.easing = 1,
            "ease-in-out" => style.easing = 2,
            "flex" => {}
            "flex-col" => style.row = false,
            "flex-row" => style.row = true,
            "flex-wrap" => style.wrap = true,
            "flex-nowrap" => style.wrap = false,
            "grow" => style.grow = 1.,
            "grow-0" => style.grow = 0.,
            "shrink" => style.shrink = 1.,
            "shrink-0" | "flex-none" => {
                style.shrink = 0.;
                if class == "flex-none" {
                    style.grow = 0.;
                }
            }
            "items-start" => style.align = Align::Start,
            "items-center" => style.align = Align::Center,
            "items-end" => style.align = Align::End,
            "items-stretch" => style.align = Align::Stretch,
            "self-auto" => style.align_self = None,
            "self-start" => style.align_self = Some(Align::Start),
            "self-center" => style.align_self = Some(Align::Center),
            "self-end" => style.align_self = Some(Align::End),
            "self-stretch" => style.align_self = Some(Align::Stretch),
            "justify-start" => style.justify = Justify::Start,
            "justify-center" => style.justify = Justify::Center,
            "justify-end" => style.justify = Justify::End,
            "justify-between" => style.justify = Justify::Between,
            "justify-around" => style.justify = Justify::Around,
            "justify-evenly" => style.justify = Justify::Evenly,
            "overflow-hidden" => style.clip = true,
            "overflow-visible" => style.clip = false,
            "whitespace-normal" => style.nowrap = Some(false),
            "whitespace-nowrap" => style.nowrap = Some(true),
            "rounded-none" => style.radius = 0.,
            "rounded" => style.radius = 4.,
            "rounded-lg" => style.radius = 8.,
            "text-sm" => style.font_size = Some(14.),
            "text-base" => style.font_size = Some(16.),
            "text-lg" => style.font_size = Some(18.),
            "text-xl" => style.font_size = Some(20.),
            "text-2xl" => style.font_size = Some(24.),
            _ => apply_value(style, class)?,
        }
    }
    Ok(())
}
fn apply_value(style: &mut Style, class: &str) -> Result<(), String> {
    if let Some(value) = class.strip_prefix("opacity-") {
        let n = finite(value)?;
        if n > 100. {
            return Err("native opacity must be 0..100".into());
        }
        style.opacity = n / 100.;
        return Ok(());
    }
    if let Some(value) = class.strip_prefix("duration-") {
        style.duration_ms = finite(value)?;
        return Ok(());
    }
    let (sign, translated) = class.strip_prefix('-').map_or((1., class), |s| (-1., s));
    for (prefix, target) in [
        ("translate-x-", &mut style.translate_x),
        ("translate-y-", &mut style.translate_y),
    ] {
        if let Some(value) = translated.strip_prefix(prefix) {
            *target = sign * spacing(value)?;
            return Ok(());
        }
    }
    for (prefix, target) in [
        ("min-w-", &mut style.min_width),
        ("max-w-", &mut style.max_width),
        ("min-h-", &mut style.min_height),
        ("max-h-", &mut style.max_height),
        ("w-", &mut style.width),
        ("h-", &mut style.height),
    ] {
        if let Some(value) = class.strip_prefix(prefix) {
            *target = length(value)?;
            return Ok(());
        }
    }
    for (prefix, target) in [("p", &mut style.padding), ("m", &mut style.margin)] {
        if let Some(value) = class.strip_prefix(prefix)
            && let Some((edge, value)) = value.split_once('-')
            && matches!(edge, "" | "x" | "y" | "t" | "r" | "b" | "l")
        {
            let value = spacing(value)?;
            match edge {
                "" => *target = Edges::all(value),
                "x" => {
                    target.left = value;
                    target.right = value;
                }
                "y" => {
                    target.top = value;
                    target.bottom = value;
                }
                "t" => target.top = value,
                "r" => target.right = value,
                "b" => target.bottom = value,
                "l" => target.left = value,
                _ => unreachable!(),
            }
            return Ok(());
        }
    }
    if let Some(value) = class.strip_prefix("gap-x-") {
        style.gap_x = spacing(value)?;
    } else if let Some(value) = class.strip_prefix("gap-y-") {
        style.gap_y = spacing(value)?;
    } else if let Some(value) = class.strip_prefix("gap-") {
        style.gap_x = spacing(value)?;
        style.gap_y = style.gap_x;
    } else if let Some(value) = class
        .strip_prefix("bg-[#")
        .and_then(|s| s.strip_suffix(']'))
    {
        style.background = Some(color(value)?);
    } else if let Some(value) = class
        .strip_prefix("text-[#")
        .and_then(|s| s.strip_suffix(']'))
    {
        style.color = Some(color(value)?);
    } else {
        return Err(format!("unsupported native utility: {class}"));
    }
    Ok(())
}
fn length(value: &str) -> Result<Length, String> {
    match value {
        "auto" => Ok(Length::Auto),
        "full" => Ok(Length::Percent(1.)),
        _ => {
            if let Some((numerator, denominator)) = value.split_once('/') {
                let n = finite(numerator)?;
                let d = finite(denominator)?;
                if d <= 0. || n > d {
                    return Err(format!("invalid native fraction: {value}"));
                }
                Ok(Length::Percent(n / d))
            } else {
                Ok(Length::Px(spacing(value)?))
            }
        }
    }
}
fn finite(value: &str) -> Result<f32, String> {
    let n = value
        .parse::<f32>()
        .map_err(|_| format!("invalid native spacing: {value}"))?;
    if n.is_finite() && (0. ..=4096.).contains(&n) {
        Ok(n)
    } else {
        Err(format!("native spacing out of range: {value}"))
    }
}
fn spacing(value: &str) -> Result<f32, String> {
    finite(value).map(|n| n * 4.)
}
fn color(value: &str) -> Result<u32, String> {
    if value.len() != 6 {
        return Err("native colors require six hex digits".into());
    }
    u32::from_str_radix(value, 16).map_err(|_| "invalid native hex color".into())
}

/// Native element defaults shared by the compiler and live component host.
pub fn for_element(tag: &str) -> Result<Style, String> {
    let mut style = Style::default();
    match tag {
        "view" => {
            style.width = Length::Percent(1.);
            style.height = Length::Percent(1.);
        }
        "div" => {}
        "span" | "p" => style.row = true,
        "button" => {
            style.row = true;
            style.padding = Edges::all(12.);
            style.radius = 6.;
            style.background = Some(0x226c65);
        }
        _ => return Err(format!("unsupported native element: {tag}")),
    }
    Ok(style)
}
