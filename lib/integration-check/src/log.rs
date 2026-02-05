#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Level {
    Critical = 50,
    Error = 40,
    Warning = 30,
    Info = 20,
    Debug = 10,
    Trace = 7,
}
