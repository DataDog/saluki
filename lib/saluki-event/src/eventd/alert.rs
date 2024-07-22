
/// EventAlertType is the alert type for events 
#[derive(Clone, Debug)]
pub enum EventAlertType{
    /// Info is the "info" AlertType for events
	Info, 

	/// Error is the "error" AlertType for events
	Error, 

	/// Warning is the "warning" AlertType for events
	Warning,

	/// Success is the "success" AlertType for events
	Success,
}