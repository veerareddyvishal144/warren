//! Mapping of `bollard` errors onto `warren_core::Error`.

use bollard::errors::Error as DockerError;
use warren_core::Error;

/// HTTP status the daemon answered with, when it answered at all.
pub(crate) fn status(e: &DockerError) -> Option<u16> {
    match e {
        DockerError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

/// `404 Not Found` from the daemon.
pub(crate) fn is_not_found(e: &DockerError) -> bool {
    status(e) == Some(404)
}

/// `409 Conflict` from the daemon.
pub(crate) fn is_conflict(e: &DockerError) -> bool {
    status(e) == Some(409)
}

/// The daemon's own message for a server error, the display form otherwise.
pub(crate) fn message(e: &DockerError) -> String {
    match e {
        DockerError::DockerResponseServerError { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// Map a Docker error. `what` names the object that was not found ("container", "image").
pub(crate) fn map(e: DockerError, what: &str) -> Error {
    match e {
        DockerError::DockerResponseServerError {
            status_code: 404,
            message,
        } => Error::not_found(what).with_details(serde_json::json!({ "docker": message })),
        DockerError::DockerResponseServerError {
            status_code: 409,
            message,
        } => Error::conflict(format!("docker: {message}")),
        DockerError::DockerResponseServerError {
            status_code: 400,
            message,
        } => Error::invalid(format!("docker rejected the request: {message}")),
        DockerError::RequestTimeoutError => {
            Error::timeout("the Docker daemon did not answer in time")
        }
        other => Error::internal(format!("docker: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(status_code: u16, message: &str) -> DockerError {
        DockerError::DockerResponseServerError {
            status_code,
            message: message.to_string(),
        }
    }

    #[test]
    fn maps_status_codes() {
        assert!(is_not_found(&server(404, "No such container")));
        assert!(is_conflict(&server(409, "busy")));
        assert!(!is_not_found(&server(500, "boom")));
        assert_eq!(
            map(server(404, "x"), "container").code,
            warren_core::errors::CODE_NOT_FOUND
        );
        assert_eq!(
            map(server(409, "x"), "container").code,
            warren_core::errors::CODE_CONFLICT
        );
        assert_eq!(
            map(server(400, "x"), "container").code,
            warren_core::errors::CODE_INVALID_REQUEST
        );
        assert_eq!(
            map(server(500, "x"), "container").code,
            warren_core::errors::CODE_INTERNAL
        );
        assert_eq!(
            map(DockerError::RequestTimeoutError, "container").code,
            warren_core::errors::CODE_TIMEOUT
        );
        assert_eq!(message(&server(500, "daemon says")), "daemon says");
    }
}
