//! Points and axis-aligned rectangles.

/// A point in the plane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f64,
    /// Vertical coordinate.
    pub y: f64,
}

/// An axis-aligned rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// The corner with the smallest coordinates.
    pub origin: Point,
    /// Extent along x.
    pub width: f64,
    /// Extent along y.
    pub height: f64,
}

/// The straight-line distance between two points.
///
/// # Arguments
///
/// * `a` - One point.
/// * `b` - The other point.
///
/// # Returns
///
/// The Euclidean distance.
pub fn distance(a: Point, b: Point) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// The point halfway between two points.
///
/// # Arguments
///
/// * `a` - One point.
/// * `b` - The other point.
///
/// # Returns
///
/// The midpoint.
pub fn midpoint(a: Point, b: Point) -> Point {
    Point {
        x: (a.x + b.x) / 2.0,
        y: (a.y + b.y) / 2.0,
    }
}

/// The area of a rectangle.
///
/// # Arguments
///
/// * `rect` - The rectangle.
///
/// # Returns
///
/// Width times height.
pub fn area(rect: &Rect) -> f64 {
    rect.width * rect.height
}

/// Whether a point lies inside a rectangle, edges included.
///
/// # Arguments
///
/// * `rect` - The rectangle.
/// * `point` - The point to test.
///
/// # Returns
///
/// `true` when the point is inside or on the edge.
pub fn contains(rect: &Rect, point: Point) -> bool {
    point.x >= rect.origin.x
        && point.x <= rect.origin.x + rect.width
        && point.y >= rect.origin.y
        && point.y <= rect.origin.y + rect.height
}
