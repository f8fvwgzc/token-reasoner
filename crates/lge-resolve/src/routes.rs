//! Matching an HTTP client call against the routes the repository exposes.
//!
//! Both sides are compared in the canonical form of `lge_core::normalize_route`,
//! where every parameter and every template hole is the segment `{}`.

/// Stands for "any method" on either side: a route registered for all methods, or
/// a call whose method the indexer could not read.
pub(crate) const ANY: &str = "ANY";

const PARAM: &str = "{}";

/// Upper-cased HTTP method; `ANY` when the indexer left it empty.
pub(crate) fn http_method(detail: &str) -> String {
    let m = detail.trim().to_uppercase();
    if m.is_empty() {
        ANY.to_string()
    } else {
        m
    }
}

pub(crate) fn methods_agree(a: &str, b: &str) -> bool {
    a == b || a == ANY || b == ANY
}

/// Segment-by-segment match of two normalized routes. A parameter in the
/// declaration matches any one segment of the call, and a hole in the call
/// matches any one declared segment. The segment counts must be equal.
///
/// A URL path is matched as a name, so at least one segment must be the same
/// written text on both sides. A call that names nothing (`https://host`,
/// `${url}`, `${BASE}/${path}`) would otherwise fit the root route or any route
/// of the same length, whatever service it really goes to.
pub(crate) fn routes_match(declared: &str, called: &str) -> bool {
    let mut d = declared.split('/').filter(|s| !s.is_empty());
    let mut c = called.split('/').filter(|s| !s.is_empty());
    let mut named = false;
    loop {
        match (d.next(), c.next()) {
            (None, None) => return named,
            (Some(x), Some(y)) if x == PARAM || y == PARAM => {}
            (Some(x), Some(y)) if x == y => named = true,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lge_core::normalize_route;

    fn matches(declared: &str, called: &str) -> bool {
        routes_match(&normalize_route(declared), &normalize_route(called))
    }

    #[test]
    fn a_declared_parameter_matches_any_single_segment() {
        assert!(matches("/orders/{order_id}", "http://orders-api/orders/42"));
        assert!(matches("/orders/:id", "/orders/42?verbose=1"));
        assert!(matches("/orders/<int:id>/items", "/orders/7/items"));
        assert!(!matches("/orders/{id}", "/orders"));
        assert!(!matches("/orders/{id}", "/orders/42/items"));
        assert!(!matches("/orders/{id}", "/customers/42"));
    }

    #[test]
    fn a_hole_in_the_call_matches_any_declared_segment() {
        assert!(matches("/orders/pending", "${BASE}/orders/${id}"));
        assert!(matches("/orders/{id}", "${BASE}/orders/${id}"));
        assert!(matches("/{tenant}/orders", "/${t}/orders"));
        assert!(!matches("/orders/pending/items", "/orders/${id}"));
    }

    #[test]
    fn a_path_with_no_written_segment_in_common_matches_nothing() {
        // The root, a host alone and a variable all normalize to `/`.
        assert!(!matches("/", "http://host"));
        assert!(!matches("", "/"));
        assert!(!matches("/", "${url}"));
        assert!(!matches("/", "/orders"));
        assert!(!matches("/{id}", "/"));
        // Only parameters and holes line up: the shape fits, no name does.
        assert!(!matches("/orders", "/${collection}"));
        assert!(!matches("/{slug}", "https://api.example.com/users"));
        assert!(!matches("/{tenant}/{page}", "${BASE}/${a}/${b}"));
        assert!(!matches("/{id}", "/{id}"));
    }

    #[test]
    fn methods_agree_when_equal_or_either_is_any() {
        assert!(methods_agree("GET", "GET"));
        assert!(methods_agree("ANY", "POST"));
        assert!(methods_agree("POST", "ANY"));
        assert!(!methods_agree("GET", "POST"));
        assert_eq!(http_method("get"), "GET");
        assert_eq!(http_method(" "), "ANY");
        assert_eq!(http_method("any"), "ANY");
    }
}
