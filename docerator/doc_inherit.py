import inspect
import re
import importlib
import functools
import textwrap
from turtledemo.penrose import start
from typing import Callable, Optional

__all__ = ["bind_signature_to_function"]

from docerator._base import REPLACE_REGEX
from docerator.parsers import PARSERS, ParameterParser

ARG_SPLIT_REGEX = re.compile(r"\s*,\s*")

def _skip_first_and_empty():
    _first_call = True
    def func(input):
        nonlocal _first_call
        result = not _first_call
        _first_call = False
        if input.isspace():
            return False
        return result

    return func

def _get_indent(target: str, docstring: str):
    # Return the indentation between target and the new line character before it.
    return re.search(rf"^\s*(?={re.escape(target)})", docstring, re.MULTILINE)[0]


def _replace_doc_args(replace_key: str, replacement: str, doc: str):
    # find the indentation
    target = f"%({replace_key})"
    indent = _get_indent(target, doc)

    # prepend indent to all lines except the first
    formatted = textwrap.indent(replacement, indent, _skip_first_and_empty())
    return doc.replace(target, formatted)

def _import_target(source_name):
    # three possibilities here for the target function.
    # (1) target is a module.Class or
    # (2) target is module.function or
    # (3) target is module.Class.function.
    try:
        # catch cases 1 and 2
        module_name, target = source_name.rsplit(".", 1)
        target = getattr(importlib.import_module(module_name), target)
    except ValueError:
        raise ValueError(
            f"{source_name} does not include the module information. "
            f"Should be included as module.to.import.from.{source_name}"
        )
    except ImportError:
        # try case 3
        try:
            module_name, class_target, func_target = source_name.rsplit(".", 2)
            target = getattr(importlib.import_module(module_name), class_target)
            target = getattr(target, func_target)
        except (ImportError, TypeError):
            raise ImportError(
                f"Unable to import class {source_name} for docstring replacement"
            )
    return target



def doc_wrap(
        doc_style: str=None,
        star_excludes: set[str]=None,
        update_signature: bool=True
) -> Callable:
    if doc_style is None:
        doc_style = 'numpydoc'
    parser = PARSERS[doc_style]

    star_excludes = set(star_excludes) if star_excludes is not None else set()
    def wrapper(func):
        if inspect.ismethod(func) or inspect.isfunction(func):
            return _doc_wrap(func, star_excludes, parser, update_signature=update_signature)
        else:
            raise TypeError("func must be a callable function or method.")
    return wrapper


def _doc_wrap(
        func: Callable,
        star_excludes: set[str],
        parser: ParameterParser,
        cls_context: Optional[type]=None,
        update_signature: bool=True
) -> tuple[str, inspect.Signature]:
    doc = func.__doc__
    if inspect.isclass(func):
        func = func.__init__
        func_name = "__init__"
    elif inspect.isfunction(func):
        func_name = func.__name__
    else:
        raise TypeError(
            f"must wrap a class or function, got a {type(func)}"
        )
    if not doc:
        return func

    # replacement items in doc
    args_to_insert = {}
    # determine which arguments to insert
    had_super = False
    had_star = False
    for match in REPLACE_REGEX.finditer(doc):
        replace_key = match.group("replace_key")
        args = []
        for item in ARG_SPLIT_REGEX.split(replace_key):
            item = item.rsplit(".", 1)
            args.append(item)
            had_super |= item[0] == 'super'
            had_star |= item[1] == '*'
        args_to_insert[replace_key] = args

    if not args_to_insert:
        return func

    signature = inspect.signature(func)
    sig_params = signature.parameters
    super_doc_dict = {}
    # build the super argument dictionary if we will need it.
    if had_super:
        if cls_context is None:
            raise ValueError("cls_context must be set for super lookups")
        for base in inspect.getmro(cls_context)[:-1][::-1]:  # don't bother checking myself or `object`
            if base_arg_dict := getattr(base, "_arg_dict", None):
                super_doc_dict.update(base_arg_dict.get(func_name, {}))

    # need to also know what was already described on myself:
    func_arg_dict = {}
    if had_star:
        if cls_context is None:
            func_arg_dict =  parser.parse_parameters(func)
        else:
            func_arg_dict = cls_context._arg_dict[func_name]

    inserted_parameters = {}
    for replace_key, args in args_to_insert.items():
        parameters = []
        for source_name, arg in args:
            # do not process * yet (or *args and **kwargs parameters)
            if arg[0] != "*":
                if source_name == "super":
                    arg_dict = super_doc_dict
                    if arg not in super_doc_dict:
                        raise TypeError(
                            f"Argument {arg} not found in {cls_context.__name__}'s inheritance tree of {func_name}."
                        )
                else:
                    target = _import_target(source_name)
                    arg_dict = getattr(target, "_arg_dict", None)
                    if arg_dict is None:
                        arg_dict = {func_name:parser.parse_parameters(target)}
                    if func_name not in arg_dict:
                        raise KeyError(
                            f"{target} does not have an argument dictionary for {func_name}"
                        )
                    arg_dict = arg_dict[func_name]
                    if arg not in arg_dict:
                        raise TypeError(
                            f"{arg}'s description not found in {target}"
                        )
                parameters.append(arg_dict[arg])
        if parameters:
            formatted = parser.format_parameter(parameters)
            doc = _replace_doc_args(replace_key, formatted, doc)
            for param in parameters:
                inserted_parameters[param.name] = param

    replaced_super_star = False
    for replace_key, args in args_to_insert.items():
        parameters = {}
        for source_name, arg in args:
            if arg[0] != "*":
                continue
            if source_name == "super":
                replaced_super_star = True
                star_arg_dict = super_doc_dict
            else:
                target = _import_target(source_name)
                star_arg_dict = getattr(target, "_arg_dict", None)
                if star_arg_dict is None:
                    star_arg_dict = {func_name:parser.parse_parameters(target)}
                if func_name not in star_arg_dict:
                    raise TypeError(f"{target} does not have {func_name} described.")
                star_arg_dict = star_arg_dict[func_name]

            # first add any parameters that were in my signature
            # to get the order right:

            for arg_name, param in sig_params.items():
                if (    arg_name not in func_arg_dict and
                        arg_name not in star_excludes and
                        arg_name not in inserted_parameters and
                        arg_name in star_arg_dict and
                        arg_name not in parameters
                ):
                    param = star_arg_dict[arg_name].replace(kind=param.kind)
                    parameters[param.name] = param

            for arg_name, param in star_arg_dict.items():
                # for each argument name, check if it was already added.
                # (or is already in this class's arg_dict for the function.)
                if (
                        arg_name not in star_excludes and
                        arg_name not in func_arg_dict and
                        arg_name not in inserted_parameters and
                        arg_name not in parameters
                ):
                    parameters[param.name] = param

        if parameters:
            # build up the replacement string
            formatted = "\n".join(parser.format_parameter(par) for par in parameters.values())
            doc = _replace_doc_args(replace_key, formatted, doc)

            for param in parameters.values():
                inserted_parameters[param.name] = param


    if update_signature:
        var_kwarg = None
        new_params = []
        # filter out the variational keyword argument
        for arg, param in sig_params.items():
            if param.kind == inspect.Parameter.VAR_KEYWORD:
                var_kwarg = param
            elif param.name in star_excludes:
                # if this parameter was meant to be excluded, skip over it.
                continue
            else:
                if param.name in inserted_parameters:
                    # update it with the inherited parameters
                    # and be sure not to change the kind of the parameter
                    # or its default
                    param = inserted_parameters[param.name].replace(kind=param.kind, default=param.default)

                new_params.append(param)
        for param in inserted_parameters.values():
            if param.name not in sig_params and var_kwarg:
                new_params.append(
                    param.replace(kind=inspect.Parameter.KEYWORD_ONLY)
                )
        # If I had a variation keyword argument, and I did not do a super.* include
        # add the variational keyword argument back in
        if var_kwarg and not replaced_super_star:
            new_params.append(var_kwarg)
        signature = inspect.Signature(parameters=new_params)

    func = bind_signature_to_function(signature, func)
    func.__doc__ = doc
    return func


def bind_signature_to_function(
    signature: inspect.Signature, func: Callable
) -> Callable:
    """Binds a callable function to a new signature.

    Parameters
    ----------
    signature : inspect.Signature
        The new signature to bind the function to.
    func : callable
        The function to bind the new signature to.

    Returns
    -------
    wrapped : callable
        The wrapped function will raise a `TypeError` if the inputs do not match
        the new signature.
    """

    # Note this function will not raise a `TypeError`, but the function returned
    # from this function will. Thus, `TypeError` is not included in the Raises doc section.
    @functools.wraps(func)
    def bind_signature(*args, **kwargs):
        try:
            params = signature.bind(*args, **kwargs)
        except TypeError as err:
            raise TypeError(f"{func.__qualname__}(): {err}") from None
        return func(*params.args, **params.kwargs)

    bind_signature.__signature__ = signature
    return bind_signature


class DoceratorMeta(type):
    """Metaclass that implements class constructor argument replacement.

    When a target class uses this as a metaclass, it will trigger a replacement on that
    target class's docstring for specific keys. It looks for replacement strings of the form:

    >>> "%(class_name.arg)"

    ``class_name`` can be either:
        1) A specific class from the target class's inheritance tree, in which case it must be in the format:
           ``f"%({class.__module__}.{class.__qualname__})``, or
        2) the special name ``super``, which triggers a lookup for the first instance
           of ``arg`` in the target class's method resolution order.

    ``arg`` can be either:
        1) A specific argument, or
        2) the ``*`` character, which will include everything from ``class_name``, except for
           arguments in ``star_excludes`` or already in the target class's ``__init__`` signature.

    It will then also adjust the target class's ``__init__`` signature to match the arguments added
    to the docstring by DoceratorMeta. If the target class had a ``**kwargs`` and a ``%(super.*)`` style
    replacement was added, the ``**kwargs`` will be removed from the signature.

    Notes
    -----
    This metaclass assumes that the target class's docstring follows the numpydoc style format.

    Any parameter that is in the target class's __init__ signature will never be pulled in with a `"*"` import.
    It is expected to either be documented on the target class's docstring or explicitly included from a parent.

    Examples
    --------
    We have a simple base class that uses the DoceratorMeta. Its subclasses then have access to the
    argument descriptions in its docstring.

    >>> from docerator import DoceratorMeta
    >>> class BaseClass(metaclass=DoceratorMeta):
    ...     '''A simple base class
    ...
    ...     Parameters
    ...     ----------
    ...     info : str
    ...         Information about this instance.
    ...
    ...     Other Parameters
    ...     ----------------
    ...     more_info : list of str, optional
    ...         Additional information
    ...     '''
    ...     def __init__(self, info, more_info=None):...

    Next we want to creat a new class that inherits from ``BaseClass`` but we don't want to copy and
    paste the description of the `item` argument. We also want to include all of the other arguments
    described in `BaseClass` in this class's Other Parameters section.
    >>> class ChildClass(BaseClass):
    ...     '''A Child Class
    ...     %(super.info)
    ...     %(super.*)
    ...     '''
    ...     def __init__(self, info, **kwargs):...
    >>> print(ChildClass.__doc__)
    A Child Class
    info : str
        Information about this instance.
    more_info : list of str, optional
        Additional information

    You can exclude arguments from wildcard includes (``*``) by setting the `star_excludes` keyword argument
    for that class.
    >>> class OtherChildClass(BaseClass, star_excludes=["more_info"]):
    ...     '''Another child class
    ...     %(super.info)
    ...     %(super.*)
    ...     '''
    ...     def __init__(self, info, **kwargs):...
    >>> print(OtherChildClass.__doc__)
    Another child class
        info : str
            Information about this instance.
    """

    def __new__(
        mcs,
        name: str,
        bases: tuple[type],
        namespace: dict,
        doc_style=None,
        star_excludes: Optional[set] = None,
        update_signature: bool = True,
        **kwargs,
    ):
        """
        Parameters
        ----------
        name : str
        bases : tuple[type]
        namespace : dict
        doc_style : str, optional
            Which documentation style this class is expected to use.
        star_excludes : set, optional
            Arguments to exclude from any (class_name.*) imports
        update_signature : bool, optional
            Whether to update the class's signature to match the updated docstring.
        **kwargs
            Extra keyword arguments passed to the parent metaclass.
        """
        # construct the class
        cls = super().__new__(mcs, name, bases, namespace, **kwargs)

        if doc_style is None:
            doc_style = 'numpydoc'
        parser = PARSERS[doc_style]

        # build the documentation argument dictionary for each of the functions
        arguments = {}
        for item_name, item in namespace.items():
            if item_name in ["__module__", "__qualname__", "__doc__"]:
                continue
            # only work with callable things (that have a signature)
            if inspect.ismethod(item) or inspect.isfunction(item):
                arguments[item_name] = parser.parse_parameters(item)
        # If this class has a `__doc__` parse its parameters (if any)
        # and add them to __init__
        if "__doc__" in namespace:
            init = arguments.get("__init__", {})
            arguments["__init__"] = init | parser.parse_parameters(cls)

        cls._arg_dict = arguments

        # Now start deciding what to replace
        if star_excludes is None:
            star_excludes = set()
        else:
            star_excludes = set(star_excludes)

        # make a copy to make sure nothing mutates the original set...
        cls._excluded_parent_args = star_excludes.copy()

        # get all the excludes from the inheritance tree.
        excludes = set()
        for base in cls.__mro__[:-1]:
            if excluded := getattr(base, "_excluded_parent_args"):
                excludes.update(excluded)

        # replace things in the docstring, and bind functions that
        # accepted **kwargs to new call signatures.
        for name, item in namespace.items():
            if name in ["__module__", "__qualname__"]:
                continue
            docstring = item.__doc__
            # If the item doesn't have a docstring, continue
            if not docstring:
                continue
            # If the docstring attribute is read-only, continue
            try:
                item.__doc__ = docstring
            except AttributeError:
                continue
            if inspect.isfunction(item):
                item = _doc_wrap(item, star_excludes, parser, cls, update_signature)
                setattr(cls, name, item)


        if "__doc__" in namespace:
            new_init = _doc_wrap(cls, star_excludes, parser, cls, update_signature)
            # skip if _doc_wrap didn't do anything...
            if new_init is not cls.__init__:
                cls.__old_doc = cls.__doc__
                cls.__doc__ = new_init.__doc__
                if update_signature:
                    # If I had an __init__ method, it would've been modified above
                    # so pull it's docstring into this function.
                    new_init.__doc__ = None
                    cls.__init__ = new_init

        return cls


# Could also add this functionality as a wrapper for a class.
