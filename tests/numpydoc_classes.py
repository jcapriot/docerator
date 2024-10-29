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

    def a_function(self, x: float, whats_this: str) -> str:
        """This is a simple function

        With two simple parameters

        Parameters
        ----------
        x : float
            The float
        whats_this : str
            The string.
        """
        ...

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

    def another_func(self, whats_this: str, its_nothing=None) -> bool:
        """Returns what is this?

        Parameters
        ----------
        whats_this : str
            String to query?
        its_nothing : bool, optional
            Is `whats_this` nothing?
        """
        return True

class GrandchildClass(
    ChildClass,
    star_excludes={
        "but_not_too_much",
    },
):
    """Docstring

    Parameters
    ----------
    %(numpydoc_classes.Parent.arg1)
    a_new_arg : dict
        Still a dictionary...
    %(super.arg2)

    Other Parameters
    ----------------
    %(numpydoc_classes.Parent.*)
    """

class CousinClass(ChildClass):
    """Kinda related docstring

    I have a bit of a summary here, but I don't want to put my parameter
    descriptions here just yet, I want to put them in __init__
    """

    def __init__(self, arg1, a_new_arg, **kwargs):
        """This is where I am created.

        Parameters
        ----------
        %(super.*)
        """

    def a_function(self, x, whats_this):
        """Return something

        Parameters
        ----------
        %(super.x)
        %(super.whats_this)

        Returns
        -------
        b : str
             The output
        """

    def another_func(self, whats_this: str, its_nothing=None, or_isit=False):
        """
        This wasn't any good...

        Parameters
        ----------
        %(super.whats_this)
        %(super.its_nothing)
        or_isit : bool, optional
            It is actualy something

        Returns
        -------
        bool
            I'm returning
        """
        return False