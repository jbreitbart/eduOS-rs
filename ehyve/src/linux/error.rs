use std::{result, fmt};
//use nix::errno;

pub type Result<T> = result::Result<T, Error>;

#[derive(Debug, Clone)]
pub enum Error {
    InternalError,
    NotEnoughMemory,
    InvalidFile(String),
	UnknownExitReason(::linux::kvm::Exit),
	KVMInitFailed,
	KVMUnableToCreateVM,
	MissingFrequency,
	ParseMemory
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::InternalError => write!(f, "An internal error has occurred, please report."),
            Error::NotEnoughMemory => write!(f, "The host system has not enough memory, please check your memory usage."),
            Error::InvalidFile(ref file) => write!(f, "The file {} was not found or is invalid.", file),
			Error::UnknownExitReason(ref exit_reason) => write!(f, "Unknowm exit reason {:?}.", exit_reason),
			Error::KVMInitFailed => write!(f, "Unable to initialize KVM"),
			Error::KVMUnableToCreateVM => write!(f, "Unable to create VM"),
			Error::MissingFrequency => write!(f, "Couldn't get the CPU frequency from your system. (is /proc/cpuinfo missing?)"),
			Error::ParseMemory => write!(f, "Couldn't parse the guest memory size from the environment")
        }
    }
}
