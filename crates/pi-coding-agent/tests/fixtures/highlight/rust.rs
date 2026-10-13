use std::collections::HashMap;

/// A point.
#[derive(Debug, Clone)]
pub struct Point<T> {
    x: T,
    y: T,
}

impl Point<i32> {
    pub fn new(x: i32, y: i32) -> Self {
        // Build it
        let mut map: HashMap<&str, u8> = HashMap::new();
        map.insert("a", 0xFF);
        println!("{} {}", x, 1.5);
        Self { x, y }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let p = Point::new(1, 2);
    if p.x > 0 && true { return Ok(()); }
    Err("bad".into())
}
