pub struct Vec3<T> {
    pub x: T,
    pub y: T,
    pub z: T,
}

impl<T: std::ops::Add<Output = T>> Vec3<T> {
    pub fn add(self, other: Vec3<T>) -> Vec3<T> {
        Vec3 { x: self.x + other.x, y: self.y + other.y, z: self.z + other.z }
    }
}

pub type Vec3f = Vec3<f32>;
pub type HashMap<K, V> = std::collections::HashMap<K, V>;
