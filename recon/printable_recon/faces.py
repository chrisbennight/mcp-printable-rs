"""Decode untrusted photographs, find the face and cut the network inputs.

Images are decoded with a pixel limit and normalised to upright RGB. MediaPipe's
face landmarker gives 478 landmarks. From them come three crops:

- a 224 pixel square for SMIRK, sized from the landmarks' extent by 1.4,
- a 224 pixel square for DECA, sized as DECA does from the extent of its 68
  facial landmarks, by 1.1 and then 1.25,
- a 112 pixel ArcFace crop for MICA, aligned by a similarity transform of five
  keypoints to the standard ArcFace template.

MediaPipe's multiclass selfie segmenter then labels a wider square around the
face, taking in the hair and neck, as background, hair, body skin, face skin,
clothes or other.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import cv2
import numpy
from PIL import Image, ImageOps, UnidentifiedImageError

# Photographs larger than this are refused rather than decoded.
MAX_PIXELS = 40_000_000
ACCEPTED_FORMATS = {"JPEG", "PNG", "WEBP"}
Image.MAX_IMAGE_PIXELS = MAX_PIXELS

# MediaPipe face mesh indices of the 68 landmarks DECA's crop was sized from:
# jaw, brows, nose, eyes and lips in the usual order.
LANDMARKS_68 = [
    127, 234, 93, 132, 58, 136, 150, 176, 152, 400, 379, 365, 288, 361, 323, 454, 356,
    70, 63, 105, 66, 107, 336, 296, 334, 293, 300,
    168, 197, 5, 4, 75, 97, 2, 326, 305,
    33, 160, 158, 133, 153, 144, 362, 385, 387, 263, 373, 380,
    61, 39, 37, 0, 267, 269, 291, 405, 314, 17, 84, 181,
    78, 82, 13, 312, 308, 317, 14, 87,
]
# The ArcFace keypoints are the pupils, the nose tip and the mouth corners.
_IRIS_CENTRES = (468, 473)
_NOSE_TIP = 4
_MOUTH_CORNERS = (61, 291)

# Five-point template of the ArcFace training crops, for 112 by 112 pixels:
# eyes, nose tip and mouth corners, left to right in the image.
ARCFACE_TEMPLATE = numpy.array(
    [
        [38.2946, 51.6963],
        [73.5318, 51.5014],
        [56.0252, 71.7366],
        [41.5493, 92.3655],
        [70.7299, 92.2041],
    ]
)


# The segmenter's classes, in the order of its labels.
SEGMENT_CLASSES = ("background", "hair", "body_skin", "face_skin", "clothes", "other")
HAIR = SEGMENT_CLASSES.index("hair")
# The segmenter works at this resolution, so the square it labels is cut to it.
SEGMENT_SIDE = 256
# The labelled square spans this multiple of the face's landmark extent, so it
# reaches the hair above the face and the neck below it.
SEGMENT_SCALE = 2.4


class FaceError(Exception):
    """An image cannot be read or has no usable face."""


@dataclass(frozen=True)
class Crop:
    """Where a 224 pixel crop came from: crop pixel p is image pixel
    p / step + (left, top)."""

    left: float
    top: float
    step: float


@dataclass(frozen=True)
class Segmentation:
    """Segmenter labels for a square around the face: label pixel p is image
    pixel p / crop.step + (crop.left, crop.top). `hair_rgb` is the median
    colour of the pixels labelled hair, if any are."""

    labels: numpy.ndarray
    crop: Crop
    hair_rgb: tuple[int, int, int] | None
    hair_pixels: int


@dataclass(frozen=True)
class Face:
    """One face: its landmarks in pixels, the three crops, where the SMIRK
    crop came from, the image's width and height and, once segmented, the
    labels around it."""

    landmarks: numpy.ndarray
    smirk: numpy.ndarray
    deca: numpy.ndarray
    arcface: numpy.ndarray
    smirk_crop: Crop
    image_size: tuple[int, int]
    segmentation: Segmentation | None = field(default=None)


def decode(path: Path) -> numpy.ndarray:
    """An upright RGB image as an array of shape (height, width, 3)."""
    try:
        with Image.open(path) as image:
            if image.format not in ACCEPTED_FORMATS:
                raise FaceError(f"{path.name} is not a JPEG, PNG or WebP image")
            if image.width * image.height > MAX_PIXELS:
                raise FaceError(f"{path.name} has more than {MAX_PIXELS} pixels")
            image.draft("RGB", image.size)
            upright = ImageOps.exif_transpose(image).convert("RGB")
            return numpy.asarray(upright, dtype=numpy.uint8).copy()
    except (UnidentifiedImageError, OSError, Image.DecompressionBombError) as error:
        raise FaceError(f"{path.name} could not be decoded: {error}") from error


class Landmarker:
    """MediaPipe's face landmarker, choosing the largest face in an image."""

    def __init__(self, model: Path) -> None:
        from mediapipe.tasks import python
        from mediapipe.tasks.python import vision

        options = vision.FaceLandmarkerOptions(
            base_options=python.BaseOptions(model_asset_path=str(model)),
            num_faces=4,
            min_face_detection_confidence=0.3,
            min_face_presence_confidence=0.3,
        )
        self._detector = vision.FaceLandmarker.create_from_options(options)

    def close(self) -> None:
        self._detector.close()

    def __call__(self, image: numpy.ndarray) -> numpy.ndarray:
        import mediapipe

        result = self._detector.detect(mediapipe.Image(image_format=mediapipe.ImageFormat.SRGB, data=image))
        if not result.face_landmarks:
            raise FaceError("no face was found")
        height, width = image.shape[:2]
        faces = [
            numpy.array([[point.x * width, point.y * height] for point in landmarks])
            for landmarks in result.face_landmarks
        ]
        return max(faces, key=lambda points: numpy.ptp(points[:, 0]) * numpy.ptp(points[:, 1]))


class Segmenter:
    """MediaPipe's multiclass selfie segmenter, labelling a square around a face."""

    def __init__(self, model: Path) -> None:
        from mediapipe.tasks import python
        from mediapipe.tasks.python import vision

        options = vision.ImageSegmenterOptions(
            base_options=python.BaseOptions(model_asset_path=str(model)),
            output_category_mask=True,
            output_confidence_masks=False,
        )
        self._segmenter = vision.ImageSegmenter.create_from_options(options)

    def close(self) -> None:
        self._segmenter.close()

    def labels(self, square: numpy.ndarray) -> numpy.ndarray:
        """A label for each pixel of an RGB square."""
        import mediapipe

        result = self._segmenter.segment(mediapipe.Image(image_format=mediapipe.ImageFormat.SRGB, data=square))
        labels = numpy.asarray(result.category_mask.numpy_view(), dtype=numpy.uint8).reshape(square.shape[:2])
        if labels.max(initial=0) >= len(SEGMENT_CLASSES):
            raise FaceError("the segmenter returned an unknown label")
        return labels

    def __call__(self, image: numpy.ndarray, landmarks: numpy.ndarray) -> Segmentation:
        return segment(image, landmarks, self.labels)


def segment(image: numpy.ndarray, landmarks: numpy.ndarray, labeller) -> Segmentation:
    """Label a square around the face with `labeller` and measure the hair."""
    square, crop = _square(image, *_extent(landmarks), SEGMENT_SCALE, side=SEGMENT_SIDE)
    labels = labeller(square)
    hair = square[labels == HAIR]
    hair_rgb = tuple(int(value) for value in numpy.median(hair, axis=0)) if len(hair) else None
    return Segmentation(labels=labels, crop=crop, hair_rgb=hair_rgb, hair_pixels=int(len(hair)))


def crops(image: numpy.ndarray, landmarks: numpy.ndarray) -> Face:
    """Cut the SMIRK, DECA and ArcFace inputs for one face."""
    if landmarks.shape != (478, 2) or not numpy.isfinite(landmarks).all():
        raise FaceError("face landmarks are incomplete")
    smirk, smirk_crop = _square(image, *_extent(landmarks), 1.4)
    left, right, top, bottom = _extent(landmarks[LANDMARKS_68])
    deca, _ = _square(image, left, right, top, bottom, 1.1 * 1.25)
    return Face(
        landmarks=landmarks,
        smirk=smirk,
        deca=deca,
        arcface=_arcface(image, landmarks),
        smirk_crop=smirk_crop,
        image_size=(image.shape[1], image.shape[0]),
    )


def _extent(points: numpy.ndarray) -> tuple[float, float, float, float]:
    return points[:, 0].min(), points[:, 0].max(), points[:, 1].min(), points[:, 1].max()


def _square(
    image: numpy.ndarray, left: float, right: float, top: float, bottom: float, scale: float,
    side: int = 224,
) -> tuple[numpy.ndarray, Crop]:
    """A square of `side` pixels about the box centre, 224 as the networks
    were trained."""
    size = int((right - left + bottom - top) / 2 * scale)
    if size < 16:
        raise FaceError("the face is too small to reconstruct")
    centre_x, centre_y = (left + right) / 2, (top + bottom) / 2
    step = (side - 1) / size
    matrix = numpy.array(
        [
            [step, 0.0, -step * (centre_x - size / 2)],
            [0.0, step, -step * (centre_y - size / 2)],
        ]
    )
    crop = Crop(left=float(centre_x - size / 2), top=float(centre_y - size / 2), step=step)
    return cv2.warpAffine(image, matrix, (side, side), flags=cv2.INTER_LINEAR, borderValue=(0, 0, 0)), crop


def _arcface(image: numpy.ndarray, landmarks: numpy.ndarray) -> numpy.ndarray:
    eyes = sorted([landmarks[index] for index in _IRIS_CENTRES], key=lambda point: point[0])
    mouth = sorted([landmarks[index] for index in _MOUTH_CORNERS], key=lambda point: point[0])
    keypoints = numpy.array([eyes[0], eyes[1], landmarks[_NOSE_TIP], mouth[0], mouth[1]])
    return cv2.warpAffine(image, similarity(keypoints, ARCFACE_TEMPLATE), (112, 112), borderValue=(0, 0, 0))


def similarity(source: numpy.ndarray, target: numpy.ndarray) -> numpy.ndarray:
    """The 2 by 3 least-squares similarity transform from source to target
    points (Umeyama's method), as insightface aligns its crops."""
    source_mean, target_mean = source.mean(axis=0), target.mean(axis=0)
    source_centred, target_centred = source - source_mean, target - target_mean
    covariance = target_centred.T @ source_centred / len(source)
    u, singular, vt = numpy.linalg.svd(covariance)
    sign = numpy.eye(2)
    if numpy.linalg.det(u) * numpy.linalg.det(vt) < 0:
        sign[1, 1] = -1
    rotation = u @ sign @ vt
    variance = (source_centred**2).sum() / len(source)
    if variance == 0:
        raise FaceError("face keypoints coincide")
    scale = (singular * numpy.diag(sign)).sum() / variance
    translation = target_mean - scale * rotation @ source_mean
    return numpy.hstack([scale * rotation, translation[:, None]])
