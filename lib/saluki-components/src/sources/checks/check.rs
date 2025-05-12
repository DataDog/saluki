use std::time::Duration;

use saluki_error::GenericError;

pub trait Check {
    fn run(&self) -> Result<(), GenericError>;
    fn interval(&self) -> Duration;
    fn id(&self) -> &str;
}
