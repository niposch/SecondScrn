//! Monitor names and layout from KDE Plasma's `kscreen-doctor`.

/// A monitor in the desktop's logical coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The enabled monitors, or `None` when `kscreen-doctor` isn't available
/// (not KDE Plasma).
pub fn outputs() -> Option<Vec<Output>> {
    let out = std::process::Command::new("kscreen-doctor").arg("-j").output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

fn parse(json: &str) -> Option<Vec<Output>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut list = Vec::new();
    for o in v.get("outputs")?.as_array()? {
        if !o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false) {
            continue;
        }
        let name = o.get("name")?.as_str()?.to_string();
        let pos = o.get("pos")?;
        let (x, y) = (pos.get("x")?.as_f64()?, pos.get("y")?.as_f64()?);
        let current =
            o.get("currentModeId").and_then(|m| m.as_str().map(str::to_string).or(m.as_i64().map(|i| i.to_string())));
        let mode = o.get("modes")?.as_array()?.iter().find(|m| {
            let id = m.get("id").and_then(|i| i.as_str().map(str::to_string).or(i.as_i64().map(|n| n.to_string())));
            id.is_some() && id == current
        });
        let size = mode.and_then(|m| m.get("size")).or_else(|| o.get("size"))?;
        let (mut w, mut h) = (size.get("width")?.as_f64()?, size.get("height")?.as_f64()?);
        // Rotation flags: 1 none, 2 left (90°), 4 inverted, 8 right (270°).
        if matches!(o.get("rotation").and_then(|r| r.as_i64()), Some(2 | 8)) {
            std::mem::swap(&mut w, &mut h);
        }
        let scale = o.get("scale").and_then(|s| s.as_f64()).filter(|s| *s > 0.0).unwrap_or(1.0);
        list.push(Output { name, x, y, width: w / scale, height: h / scale });
    }
    Some(list)
}

/// The bounding box of all monitors (where an unmapped tablet's absolute
/// range lands).
pub fn workspace(outputs: &[Output]) -> Option<Output> {
    let x0 = outputs.iter().map(|o| o.x).fold(f64::INFINITY, f64::min);
    let y0 = outputs.iter().map(|o| o.y).fold(f64::INFINITY, f64::min);
    let x1 = outputs.iter().map(|o| o.x + o.width).fold(f64::NEG_INFINITY, f64::max);
    let y1 = outputs.iter().map(|o| o.y + o.height).fold(f64::NEG_INFINITY, f64::max);
    (x1 > x0 && y1 > y0).then(|| Output { name: String::new(), x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kscreen_doctor_json() {
        let json = r#"{"outputs":[
            {"name":"eDP-1","enabled":true,"pos":{"x":0,"y":0},"currentModeId":"1","scale":1.5,"rotation":1,
             "modes":[{"id":"1","size":{"width":2880,"height":1800}}]},
            {"name":"Virtual-SecondScrn","enabled":true,"pos":{"x":1920,"y":0},"currentModeId":2,"scale":1,"rotation":2,
             "modes":[{"id":2,"size":{"width":3200,"height":2136}}]},
            {"name":"HDMI-A-1","enabled":false,"pos":{"x":0,"y":0},"modes":[]}
        ]}"#;
        let o = parse(json).unwrap();
        assert_eq!(o.len(), 2);
        assert_eq!(o[0], Output { name: "eDP-1".into(), x: 0.0, y: 0.0, width: 1920.0, height: 1200.0 });
        assert_eq!((o[1].width, o[1].height), (2136.0, 3200.0), "rotated");
        let ws = workspace(&o).unwrap();
        assert_eq!((ws.x, ws.y, ws.width, ws.height), (0.0, 0.0, 4056.0, 3200.0));
    }
}
