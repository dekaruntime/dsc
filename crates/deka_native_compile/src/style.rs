use deka_native_ir::Style;

pub fn apply(style: &mut Style, classes: &str) -> Result<(), String> {
    for class in classes.split_whitespace() {
        match class {
            "flex" | "flex-col" => style.row = false,
            "flex-row" => style.row = true,
            "rounded" => style.radius = 4.,
            "rounded-lg" => style.radius = 8.,
            "text-sm" => style.font_size = Some(14.),
            "text-base" => style.font_size = Some(16.),
            "text-lg" => style.font_size = Some(18.),
            "text-xl" => style.font_size = Some(20.),
            "text-2xl" => style.font_size = Some(24.),
            _ => {
                if let Some(v) = class.strip_prefix("p-") {
                    style.padding = spacing(v)?;
                } else if let Some(v) = class.strip_prefix("gap-") {
                    style.gap = spacing(v)?;
                } else if let Some(v) = class.strip_prefix("w-") {
                    style.width = Some(spacing(v)?);
                } else if let Some(v) = class.strip_prefix("h-") {
                    style.height = Some(spacing(v)?);
                } else if let Some(v) = class
                    .strip_prefix("bg-[#")
                    .and_then(|s| s.strip_suffix(']'))
                {
                    style.background = Some(color(v)?);
                } else if let Some(v) = class
                    .strip_prefix("text-[#")
                    .and_then(|s| s.strip_suffix(']'))
                {
                    style.color = Some(color(v)?);
                } else {
                    return Err(format!("unsupported native utility: {class}"));
                }
            }
        }
    }
    Ok(())
}
fn spacing(value: &str) -> Result<f32, String> {
    let n = value
        .parse::<f32>()
        .map_err(|_| format!("invalid native spacing: {value}"))?;
    if n.is_finite() && (0. ..=4096.).contains(&n) {
        Ok(n * 4.)
    } else {
        Err(format!("native spacing out of range: {value}"))
    }
}
fn color(value: &str) -> Result<u32, String> {
    if value.len() != 6 {
        return Err("native colors require six hex digits".into());
    }
    u32::from_str_radix(value, 16).map_err(|_| "invalid native hex color".into())
}
