XStalker
========

Python daemon that listens to Xcb Randr events, and logs activity.
Tracks which window is focused, when, and make statistics.

Status
------

WIP.
* Retrieves `WM_NAME` and `WM_CLASS`
* TODO:
	* List of categories: sequential list of matchers + category name
	* Statistical system : gather category time slice by hour slots
	* More context: cwd of pid ?
	* Update on `WM_*` change on active window ?

Install
-------

Requires:
* python >= 3.4
* xcffib python Xcb binding

Use standard distutils (--user will place it in a user local directory):

    python setup.py install [--user]
