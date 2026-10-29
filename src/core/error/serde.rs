use std::fmt::Display;

use crate::Error;

impl serde_core::de::Error for Error {
	fn custom<T: Display + ToString>(msg: T) -> Self { Self::SerdeDe(msg.to_string().into()) }
}

impl serde_core::ser::Error for Error {
	fn custom<T: Display + ToString>(msg: T) -> Self { Self::SerdeSer(msg.to_string().into()) }
}
