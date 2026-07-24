use std::future::Future;

use crate::domain::{LibraryAccess, LibraryId, Permissions};

tokio::task_local! {
    static REQUEST_ACCESS: LibraryAccess;
}

pub(crate) fn request_library() -> LibraryId {
    REQUEST_ACCESS
        .try_with(|access| access.library_id)
        .expect("library store access requires authenticated request context")
}

pub(crate) fn request_access() -> LibraryAccess {
    REQUEST_ACCESS
        .try_with(|access| *access)
        .expect("library access requires authenticated request context")
}

pub(crate) fn request_permissions() -> Permissions {
    REQUEST_ACCESS
        .try_with(|access| access.permissions)
        .expect("permission-aware access requires authenticated request context")
}

pub(crate) async fn request_access_scope<F>(access: LibraryAccess, future: F) -> F::Output
where
    F: Future,
{
    REQUEST_ACCESS.scope(access, future).await
}
