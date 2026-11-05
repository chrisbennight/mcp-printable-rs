"""Safe, actionable failures of the optional shape runtime."""


class UnavailableError(RuntimeError):
    """The host cannot run the configured inference profile."""


class NoShapeError(ValueError):
    """The photograph or sampled structure contains no usable subject."""
