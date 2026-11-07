"""Model the single offscreen-panel geometry in frontend/layout.rs and GLES Fit.

This validates geometry of an observed presentation, not its guest frame,
physical scanout, source pixels, or a second window. Arithmetic follows Rust
f32 operations and round-away-from-zero; coordinates are top-left based.
"""
import math
import struct


class GeometryError(ValueError):
    pass


def f(value):
    return struct.unpack('<f', struct.pack('<f', value))[0]


def mul(a, b):
    return f(f(a) * f(b))


def div(a, b):
    return f(f(a) / f(b))


def add(a, b):
    return f(f(a) + f(b))


def rnd(value):
    return math.floor(value + .5) if value >= 0 else math.ceil(value - .5)


def expected(layout, aspect, swapped, canvas, target):
    if layout not in ('BigTop', 'Original', 'TopOnly', 'SideBySide') or aspect not in ('Original', 'FourThree') or type(swapped) is not bool:
        raise GeometryError('unsupported layout, aspect or swap')
    for size in (canvas, target):
        if type(size) is not list or len(size) != 2 or any(type(v) is not int or not 0 < v <= 8192 for v in size):
            raise GeometryError('invalid dimensions')
    w, h = canvas
    top = [mul(240, div(5 if aspect == 'Original' else 4, 3)), 240.0]
    bottom = [320.0, 240.0]
    first, second = (bottom, top) if swapped else (top, bottom)
    def size(screen, scale):
        return [rnd(mul(n, scale)) for n in screen]
    if layout in ('BigTop', 'Original'):
        big, small = (f(1.3), f(.7)) if layout == 'BigTop' else (1., 1.)
        scale = min(div(w, max(mul(first[0], big), mul(second[0], small))), div(h, mul(240, add(big, small))))
        a, b = size(first, mul(big, scale)); c, d = size(second, mul(small, scale))
        y = int((h - b - d) / 2)
        areas = [[int((w-a)/2), y, a, b], [int((w-c)/2), y+b, c, d]]
    elif layout == 'TopOnly':
        a, b = size(first, min(div(w, first[0]), div(h, first[1])))
        areas = [[int((w-a)/2), int((h-b)/2), a, b], None]
    else:
        scale = min(div(w, add(first[0], second[0])), div(h, 240))
        a, b = size(first, scale); c, d = size(second, scale)
        x = int((w-a-c)/2)
        areas = [[x, int((h-b)/2), a, b], [x+a, int((h-d)/2), c, d]]
    if swapped:
        areas.reverse()
    tw, th = target
    scale = min(div(tw, w), div(th, h))
    left = div(add(tw, -mul(w, scale)), 2)
    offset = div(add(th, -mul(h, scale)), 2)
    def viewport(area):
        if area is None:
            return None
        x, y, width, height = area
        x1, y1 = rnd(add(left, mul(x, scale))), rnd(add(offset, mul(y, scale)))
        x2, y2 = rnd(add(left, mul(x+width, scale))), rnd(add(offset, mul(y+height, scale)))
        return [x1, y1, x2-x1, y2-y1]
    return {'fit': [scale, left, offset], 'top_crop': 0.0,
            'screens': {name: None if area is None else {'area': area, 'viewport': viewport(area)}
                        for name, area in zip(('top', 'bottom'), areas)}}


def validate_observation(profile, row):
    """Return errors for a single observation; no capture-frame identity claim."""
    try:
        scale = profile['internal_scale']
        if type(scale) not in (int, float) or not math.isfinite(scale) or not 1 <= scale <= 4 or scale * 4 != int(scale * 4):
            raise GeometryError('invalid internal scale')
        if profile['stretched'] != 'Off' or row['second_window'] is not False or row['turned'] is not False or row['layout_set'] is not True:
            raise GeometryError('unsupported presentation mode')
        if row['output'] != 'offscreen' or row['panel'] != row['target'] or row['panel'] != profile['presentation_capture']['panel_size'] or row['panel'][1] > row['panel'][0]:
            raise GeometryError('unsupported output or target')
        if type(row['sequence']) is not int or row['sequence'] <= 0:
            raise GeometryError('missing presentation identity')
        pw, ph = row['panel']
        canvas = [(480 * pw + ph // 2) // ph, 480] if pw * 3 >= ph * 4 else [640, (640 * ph + pw // 2) // pw]
        if row['layout'] != canvas:
            raise GeometryError('layout does not match panel')
        model = expected(profile['layout'], profile['aspect'], profile['swapped'], row['layout'], row['target'])
        fit = row['fit']
        if type(fit) is not list or len(fit) != 3 or any(type(x) not in (int, float) or not math.isfinite(x) for x in fit) or [f(x) for x in fit] != model['fit']:
            raise GeometryError('fit differs from modeled f32 fit')
        if type(row['top_crop']) not in (int, float) or not math.isfinite(row['top_crop']) or f(row['top_crop']) != 0:
            raise GeometryError('top crop contradicts aspect')
        for name, screen in model['screens'].items():
            observed = row.get(name)
            if screen is None:
                if name in row:
                    raise GeometryError(name + ' unexpectedly present')
            elif (not isinstance(observed, dict) or any(
                    type(observed.get(key)) is not list or len(observed[key]) != 4
                    or any(type(value) is not int for value in observed[key])
                    or observed[key] != screen[key] for key in ('area', 'viewport'))
                    or 'clip' in observed):
                raise GeometryError(name + ' area, viewport or clip differs')
        return []
    except (GeometryError, KeyError, TypeError, ValueError, OverflowError, struct.error, ZeroDivisionError) as error:
        return [str(error)]
