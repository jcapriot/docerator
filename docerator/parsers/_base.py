import inspect
import abc
from typing import Any, Optional, Union

from docerator import get_debug_level, DoceratorParsingError
from docerator._params import DescribedParameter


class ParameterParser(metaclass=abc.ABCMeta):

    @classmethod
    @abc.abstractmethod
    def doc_parameter_parser(cls, docstring: str) -> dict[str, tuple[Optional[str], Optional[str]]]:
        ...

    @classmethod
    @abc.abstractmethod
    def format_parameter(cls, param: Union[DescribedParameter, list[DescribedParameter]]) -> str:
        ...

    @classmethod
    def parse_parameters(cls, method: Any) -> dict[str, DescribedParameter]:

        docstring = method.__doc__
        # build a dictionary of argument names and their corresponding Parameter
        out_dict = {}
        if not docstring:
            return out_dict
        described_params = cls.doc_parameter_parser(docstring)

        signature = inspect.signature(method)
        func_params = signature.parameters

        # First, add all the parameters from my call signature that were described.
        for param in func_params.values():
            description = described_params.pop(param.name, None)
            if description:
                type_desc, long_desc = description
                described_param = DescribedParameter.from_inspect_param(param, type_description=type_desc,
                                                                        long_description=long_desc)
                out_dict[described_param.name] = described_param

        debug_level = get_debug_level()

        for name, description in described_params.items():
            # If I'm debugging and there are leftover described parameters, emit an error
            if debug_level:
                raise DoceratorParsingError(
                    f"Documented argument {name} is not in the signature of {method.__name__}"
                )
            # otherwise add an argument with few no details about its default value or annotation.
            param = DescribedParameter(
                name, kind=inspect.Parameter.KEYWORD_ONLY,
                type_description=description[0], long_description=description[1]
            )
            out_dict[name] = param
        return out_dict

