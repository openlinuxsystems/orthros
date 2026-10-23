column(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

column - put lines in columns, or line them up as a table

# SYNOPSIS

*column* [*-x*] [*-c* _width_] [_file_ ...]

*column* *-t* [_option_ ...] [_file_ ...]

# DESCRIPTION

column reads the lines of each _file_, or of standard input, as
util-linux's column does. Without *-t* each line is an entry, and the
entries fill the width of the terminal in columns at tab stops, down
each column and then across, or across each row with *-x*. Empty lines
are left out.

With *-t* each line is split into cells, at runs of blanks (spaces and
tabs) or at each of the characters *-s* gives, and the cells are lined
up as a table: each column as wide as its widest cell, two spaces
between them. Columns can be named, then reordered, hidden, aligned
right, narrowed to fit the width, or shown as a tree; or the table can
be written as JSON. Widths are those on a terminal: wide characters
take two columns.

A _column_ in the options below is a name *-N* gave, or a number from
1; a list is separated by commas, and empty items in it are left out.

# OPTIONS

*-t*, *--table*
	Line the cells up as a table.

*-s*, *--separator* _characters_
	Split lines at each of these characters, not at runs of blanks.

*-o*, *--output-separator* _text_
	Put _text_ between the columns, not two spaces.

*-N*, *--table-columns* _names_
	Name the columns; the names are a heading line, unless *-d*.

*-d*, *--table-noheadings*
	No heading line.

*-O*, *--table-order* _list_
	Show the columns in this order.

*-H*, *--table-hide* _list_
	Hide these columns; *-* hides those with no name.

*-R*, *--table-right* _list_
	Align these columns right.

*-T*, *--table-truncate* _list_
	Cut the cells of these columns if the table is wider than _width_.

*-W*, *--table-wrap* _list_
	Wrap the cells of these columns over more lines if the table is
	wider than _width_.

*-l*, *--table-columns-limit* _n_
	Split each line into at most _n_ cells: the last one has the rest.

*-L*, *--keep-empty-lines*
	Keep empty lines, as rows with no cells.

*-J*, *--json*
	Write the table as JSON, with the columns' names (*-N*, which it
	needs) lowercased as keys and *null* for empty cells.

*-n*, *--table-name* _name_
	The name of the JSON table (*table* by default).

*-r*, *--tree* _column_
	Show the table as a tree, drawing the lines of the tree in
	_column_; *-i* and *-p*, which it needs, say how the rows are
	related. A row whose parent is not found is a root.

*-i*, *--tree-id* _column_
	The column with each row's id.

*-p*, *--tree-parent* _column_
	The column with the id of each row's parent.

*-c*, *--output-width*, *--columns* _width_
	The width to fill, rather than the terminal's (80 when it is not
	a terminal).

*-x*, *--fillrows*
	Fill rows before columns.

*-e*, *--table-header-repeat* and *-E*, *--table-noextreme* _list_ are
taken and change nothing.

# EXIT STATUS

0, or 1 if a file could not be read or a column is not defined.

# EXAMPLES

	column -t -s : /proc/1/status
	column -t -s : -N USER,PASSWORD,UID,GID,NAME,HOME,SHELL -H PASSWORD /etc/passwd
	printf '1 0 root\\n2 1 child\\n' | column -t -N ID,PARENT,NAME -r NAME -i ID -p PARENT

# SEE ALSO

*tree*(1), *systools*(1)
