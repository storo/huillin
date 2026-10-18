"""Convierte capturas numeradas en un GIF: python scripts/make-demo-gif.py."""
import argparse
import json
from pathlib import Path
from PIL import Image

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--frames', type=Path, default=Path('dist/demo-frames'))
parser.add_argument('--output', type=Path, default=Path('docs/demo.gif'))
parser.add_argument('--duration', type=int, default=250, help='Milisegundos por captura')
parser.add_argument('--manifest', type=Path, help='JSON con entradas file y duration, en orden de montaje')
args = parser.parse_args()
sequence = json.loads(args.manifest.read_text()) if args.manifest else [
    {'file': p.name, 'duration': args.duration} for p in sorted(args.frames.glob('*.jpg'))]
paths = [args.frames / frame['file'] for frame in sequence]
if not paths:
    parser.error('No hay capturas JPG en la carpeta indicada.')
images = []
for path in paths:
    with Image.open(path) as source:
        image = source.convert('RGB')
        if image.width > 1080:
            image.thumbnail((1080, 1080), Image.Resampling.LANCZOS)
        images.append(image.quantize(colors=128))
args.output.parent.mkdir(parents=True, exist_ok=True)
images[0].save(args.output, save_all=True, append_images=images[1:],
               duration=[frame['duration'] for frame in sequence], loop=0, optimize=True, disposal=2)
with Image.open(args.output) as result:
    print(f'{args.output}: {result.n_frames} cuadros, {result.size}, {args.output.stat().st_size:,} bytes')
