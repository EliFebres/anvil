import math


def area_circle(radius):
    return math.pi * radius * radius


def area_rectangle(width, height):
    return width * height


def perimeter_rectangle(width, height):
    return 2 * (width + height)


def hypotenuse(a, b):
    return math.sqrt(a * a + b * b)


def is_square(width, height):
    return width == height


def _unused_helper():
    return None
