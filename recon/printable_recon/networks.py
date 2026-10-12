"""Network architectures for the published FLAME reconstruction checkpoints.

Each module reproduces the layer structure and parameter names of the network
that produced a checkpoint, so that the checkpoint loads exactly:

- MICA's ArcFace identity encoder (an IResNet-100) and its mapping network,
  which turns an identity embedding into 300 FLAME 2020 shape coefficients.
- SMIRK's pose, shape and expression encoders, MobileNetV3 backbones with
  linear heads, which give 50 FLAME 2020 expression coefficients and a jaw
  rotation.
- DECA's ResNet-50 encoders and its detail decoder, which turns a detail code,
  an expression and a jaw rotation into a 256 by 256 UV displacement map.
"""

from __future__ import annotations

import timm
import torch
import torch.nn.functional as functional
import torchvision
from torch import nn


class _IBasicBlock(nn.Module):
    def __init__(self, inplanes: int, planes: int, stride: int = 1, downsample: nn.Module | None = None):
        super().__init__()
        self.bn1 = nn.BatchNorm2d(inplanes, eps=1e-05)
        self.conv1 = nn.Conv2d(inplanes, planes, 3, stride=1, padding=1, bias=False)
        self.bn2 = nn.BatchNorm2d(planes, eps=1e-05)
        self.prelu = nn.PReLU(planes)
        self.conv2 = nn.Conv2d(planes, planes, 3, stride=stride, padding=1, bias=False)
        self.bn3 = nn.BatchNorm2d(planes, eps=1e-05)
        self.downsample = downsample

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        out = self.bn3(self.conv2(self.prelu(self.bn2(self.conv1(self.bn1(x))))))
        identity = self.downsample(x) if self.downsample is not None else x
        return out + identity


class ArcFace(nn.Module):
    """IResNet-100 face recognition network taking aligned 112 by 112 crops."""

    def __init__(self) -> None:
        super().__init__()
        self.inplanes = 64
        self.conv1 = nn.Conv2d(3, 64, 3, stride=1, padding=1, bias=False)
        self.bn1 = nn.BatchNorm2d(64, eps=1e-05)
        self.prelu = nn.PReLU(64)
        self.layer1 = self._layer(64, 3)
        self.layer2 = self._layer(128, 13)
        self.layer3 = self._layer(256, 30)
        self.layer4 = self._layer(512, 3)
        self.bn2 = nn.BatchNorm2d(512, eps=1e-05)
        self.dropout = nn.Dropout(p=0.0)
        self.fc = nn.Linear(512 * 7 * 7, 512)
        self.features = nn.BatchNorm1d(512, eps=1e-05)

    def _layer(self, planes: int, blocks: int) -> nn.Sequential:
        downsample = nn.Sequential(
            nn.Conv2d(self.inplanes, planes, 1, stride=2, bias=False),
            nn.BatchNorm2d(planes, eps=1e-05),
        )
        layers = [_IBasicBlock(self.inplanes, planes, 2, downsample)]
        self.inplanes = planes
        layers += [_IBasicBlock(planes, planes) for _ in range(1, blocks)]
        return nn.Sequential(*layers)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        x = self.prelu(self.bn1(self.conv1(x)))
        x = self.layer4(self.layer3(self.layer2(self.layer1(x))))
        x = torch.flatten(self.bn2(x), 1)
        return self.features(self.fc(self.dropout(x)))


class IdentityMapping(nn.Module):
    """MICA's mapping from a unit identity embedding to FLAME shape coefficients."""

    def __init__(self, hidden_layers: int = 3, shape_components: int = 300) -> None:
        super().__init__()
        self.network = nn.ModuleList(
            [nn.Linear(512, shape_components)]
            + [nn.Linear(shape_components, shape_components) for _ in range(hidden_layers)]
        )
        self.output = nn.Linear(shape_components, shape_components)

    def forward(self, embedding: torch.Tensor) -> torch.Tensor:
        hidden = embedding
        for layer in self.network:
            hidden = functional.leaky_relu(layer(hidden), negative_slope=0.2)
        return self.output(hidden)


class _SmirkEncoder(nn.Module):
    def __init__(self, backbone: str, outputs: int, head: str) -> None:
        super().__init__()
        self.encoder = timm.create_model(backbone, pretrained=False, features_only=True)
        features = self.encoder.feature_info[-1]["num_chs"]
        setattr(self, head, nn.Sequential(nn.Linear(features, outputs)))
        self._head = head

    def forward(self, image: torch.Tensor) -> torch.Tensor:
        features = self.encoder(image)[-1]
        features = functional.adaptive_avg_pool2d(features, (1, 1)).flatten(1)
        return getattr(self, self._head)(features)


class Smirk(nn.Module):
    """SMIRK's three encoders. Images are RGB in [0, 1], 224 by 224."""

    EXPRESSIONS = 50

    def __init__(self) -> None:
        super().__init__()
        self.pose_encoder = _SmirkEncoder("tf_mobilenetv3_small_minimal_100", 6, "pose_cam_layers")
        self.shape_encoder = _SmirkEncoder("tf_mobilenetv3_large_minimal_100", 300, "shape_layers")
        self.expression_encoder = _SmirkEncoder(
            "tf_mobilenetv3_large_minimal_100", self.EXPRESSIONS + 2 + 3, "expression_layers"
        )

    def forward(self, image: torch.Tensor) -> dict[str, torch.Tensor]:
        pose_camera = self.pose_encoder(image)
        expression = self.expression_encoder(image)
        n = self.EXPRESSIONS
        # The first jaw component opens the mouth and cannot be negative; the
        # others turn the jaw sideways within a small range.
        jaw = torch.cat(
            [functional.relu(expression[:, n + 2 : n + 3]), torch.clamp(expression[:, n + 3 : n + 5], -0.2, 0.2)],
            dim=-1,
        )
        return {
            "head_rotation": pose_camera[:, :3],
            # Orthographic scale and x, y offset of the head in the crop.
            "camera": pose_camera[:, 3:],
            "shape": self.shape_encoder(image),
            "expression": expression[:, :n],
            "eyelids": torch.clamp(expression[:, n : n + 2], 0.0, 1.0),
            "jaw": jaw,
        }


class DecaEncoder(nn.Module):
    """A ResNet-50 feature extractor with DECA's two-layer regression head."""

    def __init__(self, outputs: int) -> None:
        super().__init__()
        backbone = torchvision.models.resnet50(weights=None)
        backbone.fc = nn.Identity()
        self.encoder = backbone
        self.layers = nn.Sequential(nn.Linear(2048, 1024), nn.ReLU(), nn.Linear(1024, outputs))

    def forward(self, image: torch.Tensor) -> torch.Tensor:
        return self.layers(self.encoder(image))


class DecaDetailDecoder(nn.Module):
    """DECA's generator of UV displacement, in FLAME units of metres."""

    def __init__(self, latent: int = 128 + 50 + 3, scale: float = 0.01) -> None:
        super().__init__()
        self.scale = scale
        self.l1 = nn.Sequential(nn.Linear(latent, 128 * 8 * 8))

        def block(inputs: int, outputs: int) -> list[nn.Module]:
            # The second positional BatchNorm argument is eps, as in DECA.
            return [
                nn.Upsample(scale_factor=2, mode="bilinear"),
                nn.Conv2d(inputs, outputs, 3, stride=1, padding=1),
                nn.BatchNorm2d(outputs, 0.8),
                nn.LeakyReLU(0.2, inplace=True),
            ]

        self.conv_blocks = nn.Sequential(
            nn.BatchNorm2d(128),
            *block(128, 128),
            *block(128, 64),
            *block(64, 64),
            *block(64, 32),
            *block(32, 16),
            nn.Conv2d(16, 1, 3, stride=1, padding=1),
            nn.Tanh(),
        )

    def forward(self, latent: torch.Tensor) -> torch.Tensor:
        out = self.l1(latent).view(latent.shape[0], 128, 8, 8)
        return self.conv_blocks(out) * self.scale


class Deca(nn.Module):
    """DECA's coarse and detail encoders and its detail decoder."""

    # Shape, texture, expression, pose (global rotation then jaw), camera and
    # lighting, in that order.
    PARAMETERS = 100 + 50 + 50 + 6 + 3 + 27
    EXPRESSION = slice(150, 200)
    JAW = slice(203, 206)
    DETAIL = 128

    def __init__(self) -> None:
        super().__init__()
        self.E_flame = DecaEncoder(self.PARAMETERS)
        self.E_detail = DecaEncoder(self.DETAIL)
        self.D_detail = DecaDetailDecoder()

    def detail_code(self, image: torch.Tensor) -> torch.Tensor:
        return self.E_detail(image)

    def coarse_expression(self, image: torch.Tensor) -> tuple[torch.Tensor, torch.Tensor]:
        """DECA's own jaw rotation and 50 expression coefficients for a crop."""
        parameters = self.E_flame(image)
        return parameters[:, self.JAW], parameters[:, self.EXPRESSION]

    def displacement(self, jaw: torch.Tensor, expression: torch.Tensor, detail: torch.Tensor) -> torch.Tensor:
        """UV displacement for a jaw rotation, 50 expression coefficients and a detail code."""
        return self.D_detail(torch.cat([jaw, expression, detail], dim=1))
