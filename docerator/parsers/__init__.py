from typing import Dict

from ._base import ParameterParser
from ._numpydoc import NumpydocParser

PARSERS: Dict[str, ParameterParser] = {
    'numpydoc':NumpydocParser,
}