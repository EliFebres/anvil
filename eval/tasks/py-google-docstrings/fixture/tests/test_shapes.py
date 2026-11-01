import math

from shapes import area_circle, area_rectangle, hypotenuse, is_square, perimeter_rectangle


def test_shapes():
    assert math.isclose(area_circle(1.0), math.pi)
    assert area_rectangle(2.0, 3.0) == 6.0
    assert perimeter_rectangle(2.0, 3.0) == 10.0
    assert hypotenuse(3.0, 4.0) == 5.0
    assert is_square(2.0, 2.0)
    assert not is_square(2.0, 3.0)
