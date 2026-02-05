use super::Tags;

#[derive(Debug, Clone)]
pub struct Histrogram {
    pub metric_name: String,
    pub value: i64,
    pub lower_bound: f32,
    pub upper_bound: f32,
    pub monotonic: i32,
    pub tags: Tags,
}
