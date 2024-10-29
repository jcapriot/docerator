from __future__ import annotations  # used for lookahead returns
import inspect
import textwrap
from typing import Optional

class _void():
    """An empty class to mark no input."""
    pass
# note that there are custom types to describe empty defaults and annotations
# because None is a perfectly valid thing to be there.

class DescribedParameter(inspect.Parameter):

    __slots__ = inspect.Parameter.__slots__ + ('_type_description', '_long_description')

    def __init__(
            self,
            name: str,
            kind: str,
            *,
            default: object=inspect.Parameter.empty,
            annotation=inspect.Parameter.empty,
            type_description: Optional[str] = None,
            long_description: Optional[str] = None,

    ) -> None:
        super().__init__(name, kind, default=default, annotation=annotation)
        if type_description is not None and not isinstance(type_description, str):
            TypeError(
                f"type_description must be a str, not a {type(type_description).__name__}"
            )
        self._type_description = type_description
        if long_description is not None:
            if not isinstance(long_description, str):
                TypeError(
                    f"type_description must be a str, not a {type(type_description).__name__}"
                )
            long_description = textwrap.dedent(long_description)
        self._long_description = long_description

    @property
    def type_description(self) -> Optional[str]:
        return self._type_description

    @property
    def long_description(self) -> Optional[str]:
        return self._long_description

    def replace(
            self, *, name=_void, kind=_void, annotation=_void, default=_void,
            type_description=_void, long_description=_void,
    ) -> DescribedParameter:

        if name is _void:
            name = self._name

        if kind is _void:
            kind = self._kind

        if annotation is _void:
            annotation = self._annotation

        if default is _void:
            default = self._default

        if type_description is _void:
            type_description = self._type_description

        if long_description is _void:
            long_description = self._long_description

        return type(self)(
            name, kind, default=default, annotation=annotation,
            type_description=type_description,
            long_description=long_description,
        )

    def __str__(self) -> str:
        formatted = super().__str__()
        if self._type_description is not None:
            formatted += f" : {self._type_description}"
        if self._long_description is not None:
            formatted += f"\n{textwrap.indent(self._long_description, '    ')}"
        return formatted

    def __hash__(self) -> int:
        hashed = super().__hash__()
        if self._type_description is not None:
            hashed = hash((hashed, self._type_description))
        if self._long_description is not None:
            hashed = hash((hashed, self._long_description))
        return hashed

    def __eq__(self, other) -> bool:
        if self is other:
            return True
        if not isinstance(other, DescribedParameter):
            return NotImplemented
        return (self._name == other._name and
                self._kind == other._kind and
                self._default == other._default and
                self._annotation == other._annotation and
                self._type_description == other._type_description and
                self._long_description == other._long_description)

    @classmethod
    def from_inspect_param(cls, param: inspect.Parameter,
            type_description: Optional[str] = None,
            long_description: Optional[str] = None,
    ) -> DescribedParameter:
        return DescribedParameter(
            param.name, param.kind, default=param.default, annotation=param.annotation,
            type_description=type_description, long_description=long_description
        )