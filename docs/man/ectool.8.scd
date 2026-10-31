ectool(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

ectool - set the embedded controller's fans, LEDs and ARGB header

# SYNOPSIS

*ectool fanduty* [_idx_] _percent_

*ectool autofanctrl* [_idx_]

*ectool fancurve* _idx_ [_key_*=*_value_ ...]

*ectool pwmgetfanrpm* [_idx_ | *all*]

*ectool pwmgetnumfans*

*ectool led* _name_ *query* | *auto* | *off* | _color_ | _color_*=*_value_ ...

*ectool rgbkbd* _key_ _RGB_ [_RGB_ ...]

*ectool rgbkbd clear* _RGB_

*ectool help*

# DESCRIPTION

ectool sets what the machine's Chromium embedded controller (EC) keeps:
its fans' duties and, on the Framework Desktop, their curves; its LEDs;
and the addressable LEDs of its ARGB header (ADR 0208). Its commands are
those of the Chromium EC's own ectool, with the same words and output,
for what Dukovo has; *fancurve* is Dukovo's, for Framework's fan curves.

ChromeOS's ectool sends the EC any host command. This one asks the
kernel, through the platform capability (*radix-platform*(2)), which
checks each request before it reaches the EC and passes no other
command through. A program has the capability only when the service
that started it asks for *admin.hardware* (*spark*(8)); the console's
shell has it, from *consoled*(8). The fans' speeds and the sensors'
names come from *cros\_ec* in _/kernel/class/hwmon_ (*radix-sysfs*(5)),
which anyone may read.

A number may be decimal, hexadecimal after *0x*, or octal after *0*.

# COMMANDS

*fanduty* [_idx_] _percent_
	Run fan _idx_, or every fan, at _percent_ of its duty, from 0 to
	100. The fan leaves the EC's control until *autofanctrl*, or until
	the machine restarts or wakes from sleep. The kernel's log says
	which program took it.

*autofanctrl* [_idx_]
	Give fan _idx_, or every fan, back to the EC, which runs it as the
	temperatures and its curve say.

*fancurve* _idx_ [_key_*=*_value_ ...]
	Print fan _idx_'s curve, and the duty the EC gives it now; or
	change the parts of it each _key_ names, keeping the rest. Below
	*min\_temp* of sensor *sensor*, in degrees Celsius, the fan stops, or
	runs at *min\_duty* if *always\_on* is 1; from there its duty rises in
	a straight line to *max\_duty*, in percent, at *max\_temp*. The
	temperatures must rise and the duties must not fall. A curve stays
	until the EC itself restarts. Only the Framework Desktop's EC has
	curves.

*pwmgetfanrpm* [_idx_ | *all*]
	Print each fan's speed, or fan _idx_'s.

*pwmgetnumfans*
	Print how many fans the EC has.

*led* _name_ *query*
	Print how bright LED _name_ goes in each of its colors.

*led* _name_ *auto* | *off* | _color_ | _color_*=*_value_ ...
	Give LED _name_ back to the EC, turn it off, light it in one color
	at its brightest, or light it at a brightness from 0 to 255 in each
	color named. The LEDs are *battery*, *power*, *adapter*, *left*,
	*right* and *second\_power*, those the EC has; the colors are *red*,
	*green*, *blue*, *yellow*, *white* and *amber*.

*rgbkbd* _key_ _RGB_ [_RGB_ ...]
	Give the addressable LEDs from number _key_ on the colors _RGB_,
	each a number such as *0xff8000*, up to the 128th LED.

*rgbkbd clear* _RGB_
	Give all 128 addressable LEDs the color _RGB_.

*help*
	List the commands.

# EXAMPLES

```
$ ectool pwmgetfanrpm
Fan 0 RPM: 1190
Fan 1 RPM: 1214
Fan 2 RPM: 0
$ ectool fancurve 0 min_temp=45 max_temp=85
Fan 0 curve set.
$ ectool led power off
$ ectool rgbkbd clear 0x0000ff
```

# EXIT STATUS

0 when the command did what it was asked; 1 for *help*, a mistake in
the command line, a program without the platform capability, a machine
whose kernel drives no EC, or an EC that lacks the command, has no such
fan, sensor or LED, does not take the values or does not answer.

# SEE ALSO

*radix-platform*(2), *spark*(8), *consoled*(8), *radix-sysfs*(5),
*systools*(1)
