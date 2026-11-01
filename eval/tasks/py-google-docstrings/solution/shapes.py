"""Small geometry helpers."""

import math


def area_circle(radius):
    """Compute the area of a circle.

    Args:
        radius: The circle's radius.

    Returns:
        The area.
    """
    return math.pi * radius * radius


def area_rectangle(width, height):
    """Compute the area of a rectangle.

    Args:
        width: The width.
        height: The height.

    Returns:
        The area.
    """
    return width * height


def perimeter_rectangle(width, height):
    """Compute the perimeter of a rectangle.

    Args:
        width: The width.
        height: The height.

    Returns:
        The perimeter.
    """
    return 2 * (width + height)


def hypotenuse(a, b):
    """Compute the hypotenuse of a right triangle.

    Args:
        a: One leg.
        b: The other leg.

    Returns:
        The hypotenuse length.
    """
    return math.sqrt(a * a + b * b)


def is_square(width, height):
    """Tell whether a rectangle is a square.

    Args:
        width: The width.
        height: The height.

    Returns:
        True when both sides are equal.
    """
    return width == height


def _unused_helper():
    return None
