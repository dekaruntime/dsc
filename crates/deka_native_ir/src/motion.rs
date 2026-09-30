use crate::{Keyframe, Style};
fn number(s: &str, min: f32, max: f32) -> Result<f32, String> {
    s.parse::<f32>()
        .ok()
        .filter(|n| n.is_finite() && *n >= min && *n <= max)
        .ok_or_else(|| format!("invalid motion value: {s}"))
}
pub(crate) fn apply(s: &mut Style, token: &str) -> Result<bool, String> {
    let m = &mut s.motion;
    match token {
        "spring" => {
            m.stiffness = 180.;
            return Ok(true);
        }
        "spring-none" => {
            m.stiffness = 0.;
            return Ok(true);
        }
        "transition-layout" => {
            m.layout = true;
            return Ok(true);
        }
        "layout-none" => {
            m.layout = false;
            return Ok(true);
        }
        "alternate" => {
            m.alternate = true;
            return Ok(true);
        }
        "repeat-infinite" => {
            m.repeats = 0;
            return Ok(true);
        }
        "animate-none" => {
            m.frames.clear();
            return Ok(true);
        }
        _ => {}
    }
    for (prefix, target, min, max) in [
        ("spring-stiffness-", &mut m.stiffness, 1., 1000.),
        ("spring-damping-", &mut m.damping, 1., 100.),
        ("delay-", &mut m.delay_ms, 0., 10000.),
        ("stagger-", &mut m.stagger_ms, 0., 2000.),
    ] {
        if let Some(value) = token.strip_prefix(prefix) {
            *target = number(value, min, max)?;
            return Ok(true);
        }
    }
    if let Some(value) = token.strip_prefix("repeat-") {
        m.repeats = value
            .parse()
            .ok()
            .filter(|n| (1..=1000).contains(n))
            .ok_or("repeat requires an integer 1..1000")?;
        return Ok(true);
    }
    for (prefix, target) in [("enter-", &mut m.enter), ("exit-", &mut m.exit)] {
        if let Some(value) = token.strip_prefix(prefix) {
            *target = match value {
                "none" => 0,
                "fade" => 1,
                "slide" => 2,
                "scale" => 3,
                _ => return Err(format!("unsupported presence effect: {value}")),
            };
            return Ok(true);
        }
    }
    if let Some(value) = token.strip_prefix("scale-") {
        s.scale = number(value, 1., 400.)? / 100.;
        return Ok(true);
    }
    let (sign, token) = token.strip_prefix('-').map_or((1., token), |v| (-1., v));
    if let Some(value) = token.strip_prefix("rotate-") {
        s.rotate = sign * number(value, 0., 3600.)?;
        return Ok(true);
    }
    if sign < 0. {
        return Ok(false);
    }
    let preset: Option<(u8, &[f32])> = match token {
        "animate-pulse" => Some((0, &[1., 0.3, 1.])),
        "animate-bounce" => Some((2, &[0., -24., 0.])),
        "animate-shake" => Some((1, &[0., -12., 12., -8., 8., 0.])),
        "animate-spin" => Some((4, &[0., 360.])),
        _ => None,
    };
    if let Some((property, values)) = preset {
        m.frames = values
            .iter()
            .enumerate()
            .map(|(i, value)| Keyframe {
                property,
                at: i as f32 / (values.len() - 1) as f32,
                value: *value,
            })
            .collect();
        return Ok(true);
    }
    for (name, property) in [
        ("opacity", 0),
        ("x", 1),
        ("y", 2),
        ("scale", 3),
        ("rotate", 4),
    ] {
        if let Some(values) = token
            .strip_prefix(&format!("frames-{name}-["))
            .and_then(|v| v.strip_suffix(']'))
        {
            let mut frames = vec![];
            for item in values.split(',') {
                let (at, value) = item
                    .split_once(':')
                    .ok_or("keyframes require percent:value pairs")?;
                let (min, max) = match property {
                    0 => (0., 1.),
                    3 => (0.01, 4.),
                    _ => (-4096., 4096.),
                };
                frames.push(Keyframe {
                    property,
                    at: number(at, 0., 100.)? / 100.,
                    value: number(value, min, max)?,
                });
            }
            if frames.len() < 2
                || frames.len() > 32
                || frames[0].at != 0.
                || frames.last().unwrap().at != 1.
                || frames.windows(2).any(|w| w[0].at >= w[1].at)
            {
                return Err("keyframes need 2..32 increasing points from 0 to 100".into());
            }
            m.frames.retain(|f| f.property != property);
            m.frames.extend(frames);
            return Ok(true);
        }
    }
    Ok(false)
}
