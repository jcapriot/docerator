import docerator

class Parent(metaclass=docerator.DoceratorMeta):
    """A docstring

    Parameters
    ----------
    arg1 : object
        Extended Description.
    arg2 : int
        2 Extended Description.
    arg3 : int
        3 Extended Description.

    Other Parameters
    ----------------
    even_more : list
    but_not_too_much
        But another description.
    """

    def __init__(self, arg1, arg2, arg3, even_more, but_not_too_much): ...

class ChildClass(Parent):
    """Docstring

    Parameters
    ----------
    arg1 : int
        Not quite the same as parent
    a_new_arg : dict
        A dictionary.
    %(numpydoc_classes.Parent.arg2)

    Other Parameters
    ----------------
    %(super.*)
    """

    def __init__(self, arg1, a_new_arg, **kwargs): ...

class GrandchildClass(
    ChildClass,
    star_excludes={
        "but_not_too_much",
    },
):
    __doc__ = f"""Docstring

    Parameters
    ----------
    %(numpydoc_classes.Parent.arg1)
    a_new_arg : dict
        Still a dictionary..
    %(super.arg2)

    Other Parameters
    ----------------
    %(numpydoc_classes.Parent.*)
    """