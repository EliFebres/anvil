use geometry::geometry::{Point, Rect, area, contains, distance, midpoint};

#[test]
fn geometry_works() {
    let a = Point { x: 0.0, y: 0.0 };
    let b = Point { x: 3.0, y: 4.0 };
    assert_eq!(distance(a, b), 5.0);
    assert_eq!(midpoint(a, b), Point { x: 1.5, y: 2.0 });
    let rect = Rect { origin: a, width: 3.0, height: 4.0 };
    assert_eq!(area(&rect), 12.0);
    assert!(contains(&rect, Point { x: 1.0, y: 1.0 }));
    assert!(!contains(&rect, Point { x: 5.0, y: 1.0 }));
}
