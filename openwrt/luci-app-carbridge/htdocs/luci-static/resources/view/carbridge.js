// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only
'use strict';
'require view';
'require form';
'require fs';
'require poll';
'require rpc';
'require ui';
'require uci';

const RUN = '/tmp/run/carbridge/';

const CARLINK_STATES = {
	starting: _('Starting'),
	scanning: _('Looking for the head unit'),
	'pin-needed': _('Enter the PIN the head unit shows'),
	connecting: _('Connecting to the head unit'),
	connected: _('Connected to the head unit'),
	casting: _('Casting to the head unit'),
	retrying: _('Connection failed, trying again'),
	stopped: _('Stopped')
};

// How each Carlink state looks: going well, under way or waiting for the owner, and gone wrong.
const CARLINK_LOOKS = {
	connected: 'good',
	casting: 'good',
	connecting: 'wait',
	'pin-needed': 'wait',
	retrying: 'bad'
};

// Restarts a service by its init script.
const callInitAction = rpc.declare({
	object: 'luci',
	method: 'setInitAction',
	params: [ 'name', 'action' ],
	expect: { result: false }
});

// Why CarPlay cannot run, by the reason the status gives.
const UNAVAILABLE_TEXTS = {
	mfi: _('No usable MFi certificate.'),
	bluetooth: _('Bluetooth is not ready.'),
	network: _('The iPhone network interface has no IPv4 address.'),
	receiver: _('The CarPlay receiver did not start.')
};

function readStatus() {
	return L.resolveDefault(fs.read(RUN + 'status.json'), null).then(function(text) {
		try {
			return text ? JSON.parse(text) : null;
		}
		catch (e) {
			return null;
		}
	});
}

const LOG_LINES = 300;
// Where the detailed log goes unless the settings say otherwise, and what joins its files for a download.
const DETAIL_LOG_DIR = '/opt/carbridge/log';
const DETAIL_LOG_READER = '/usr/libexec/carbridge-detail-log';
// Lists the router's Wi-Fi adapters for the choice of the one that joins the head unit's network.
const CARLINK_ADAPTER = '/usr/libexec/carbridge-carlink';

// The router's Wi-Fi adapters, as { mac, phy, driver, bus, iface, ap, openwrt }; none when they cannot be listed.
function readAdapters() {
	return L.resolveDefault(fs.exec_direct(CARLINK_ADAPTER, [ 'list' ], 'json'), null).then(function(list) {
		return (list && Array.isArray(list.adapters)) ? list.adapters : [];
	});
}

// How an adapter shows in the choice: its MAC address, then its driver, where it is connected and its interface.
function adapterText(adapter) {
	const parts = [ adapter.driver || _('unknown driver'), adapter.bus === 'usb' ? _('USB') : _('built in') ];
	if (adapter.iface)
		parts.push(adapter.iface);
	let text = '%s (%s)'.format(adapter.mac, parts.join(', '));
	if (adapter.openwrt)
		text += ' – ' + _('in the router\'s Wi-Fi, which will leave it');
	return text;
}

// The last `count` lines of the log, `LOG_LINES` unless given, from the file and the one it rotated to before.
function readLog(count) {
	return Promise.all([
		L.resolveDefault(fs.read(RUN + 'carbridge.log.1'), ''),
		L.resolveDefault(fs.read(RUN + 'carbridge.log'), '')
	]).then(function(parts) {
		const lines = (parts[0] + parts[1]).split('\n').filter(function(line) { return line !== ''; });
		return lines.slice(-(count || LOG_LINES)).join('\n');
	});
}

// Whether the settings turn the detailed log on.
function detailLogOn() {
	return uci.get('carbridge', 'main', 'detail_log') === '1';
}

// Hands `blob` to the browser as a file called `name`.
function saveBlob(blob, name) {
	const link = E('a', { 'href': URL.createObjectURL(blob), 'download': name });
	document.body.appendChild(link);
	link.click();
	link.remove();
	window.setTimeout(function() { URL.revokeObjectURL(link.href); }, 10000);
}

// A head unit's ID as it is shown: an address with colons, a car ID as it is.
function headUnitId(id) {
	return /^[0-9a-f]{12}$/.test(id) ? id.toUpperCase().match(/../g).join(':') : id;
}

// The size the automatic picture size gives a head unit that reported `reported`: the full screen view its touches
// measured, for a picture of the shape of the area it reported, else that area, as WxH.
function automaticSize(reported, views) {
	const area = reported.width + 'x' + reported.height;
	const measured = views && views.full_screen;
	const ratio = function(size) {
		const m = /^([0-9]+)x([0-9]+)$/.exec(size || '');
		return m && +m[2] > 0 ? +m[1] / +m[2] : 0;
	};
	if (!measured || !ratio(measured.picture) || Math.abs(ratio(area) / ratio(measured.picture) - 1) > 0.01)
		return area;
	return measured.view;
}

// A head unit as it is named to the owner: by its name, else by its ID.
function headUnitName(unit) {
	return unit.name || headUnitId(unit.id);
}

// A picture size CarPlay takes as it is, as WxH: even sides, the long one 320 to 3840 and the short one 240 to 2160.
// Null for anything else.
function checkSize(value) {
	const m = /^([0-9]+)x([0-9]+)$/.exec(value || '');
	if (!m)
		return null;
	const width = +m[1], height = +m[2];
	const long = Math.max(width, height), short = Math.min(width, height);
	if (width % 2 || height % 2 || long < 320 || long > 3840 || short < 240 || short > 2160)
		return null;
	return width + 'x' + height;
}

const SIZE_ERROR = _('Even width x height in pixels, from 320x240 to 3840x2160, for example 1280x720');

// A density CarPlay takes, in dots per inch from 72 to 640, as a string; null for anything else.
function checkDpi(value) {
	if (!/^[0-9]+$/.test(value || ''))
		return null;
	const dpi = +value;
	return dpi >= 72 && dpi <= 640 ? String(dpi) : null;
}

const DPI_ERROR = _('A density in dots per inch, from 72 to 640');

function sizeText(value) {
	return String(value).replace('x', '×');
}

const HEVC_TEXTS = {
	off: _('Off'),
	auto: _('When the head unit offers it'),
	force: _('Always')
};

const START_TEXTS = {
	fullscreen: _('Full screen'),
	window: _('In the window')
};

const NAV_AUDIO_TEXTS = {
	tts: _('Navigation speech'),
	media: _('Media'),
	call: _('Phone call'),
	assistant: _('AI assistant')
};

const NAV_AUDIO_HINT = [ _('The kind of sound the iPhone\'s navigation prompts play as on the head unit.'), _('The head unit then treats them as that kind of sound: its volume, how other sound is lowered, and audio focus.'), _('A change applies from the next prompt.') ].join('\n');

// The option that is the default, marked as such.
function defaultText(text) {
	return _('[Default] %s').format(text);
}

// The defaults for head units without settings of their own, as the settings below have them.
function defaults() {
	const get = function(option, fallback) {
		return uci.get('carbridge', 'main', option) || fallback;
	};
	return {
		picture: get('carplay_picture', 'auto'),
		size: get('carplay_size', '1280x720'),
		fps: get('carplay_fps', 'auto'),
		hevc: get('hevc', 'auto'),
		start: get('carplay_start', 'fullscreen'),
		nav: get('nav_audio', 'tts')
	};
}

function select(choices, current) {
	return E('select', { 'class': 'cbi-input-select' }, choices.map(function(choice) {
		return E('option', { 'value': choice[0], 'selected': choice[0] === current ? '' : null }, choice[1]);
	}));
}

// The choices of a setting whose values and their texts are `values`, in order: the default one first, marked,
// under 'default', then the others.
function choicesAround(values, fallback) {
	const own = values.filter(function(value) { return value[0] === fallback; })[0];
	return [ [ 'default', defaultText(own ? own[1] : fallback) ] ].concat(values.filter(function(value) {
		return value[0] !== fallback;
	}));
}

// The values and texts of `texts`, in order.
function pairs(texts) {
	return Object.keys(texts).map(function(value) { return [ value, texts[value] ]; });
}

// The choice a head unit's own value selects: the default one when it has none, or the one it has equals the default.
function chosen(own, fallback) {
	return own && own !== fallback ? own : 'default';
}

// The density of a head unit's screen, as it reports it: its real one when that is one CarPlay takes, and the one it
// gives the phone to lay out with.
function reportedDensities(reported) {
	return {
		real: reported ? checkDpi(String(reported.real_dpi || '')) : null,
		configured: reported && reported.dpi > 0 ? String(reported.dpi) : null
	};
}

// The diagonal in inches of the head unit's screen at `dpi`, to one decimal; null when its size is not known.
function screenInches(reported, dpi) {
	if (!reported || !(reported.screen_width > 0) || !(reported.screen_height > 0) || !(+dpi > 0))
		return null;
	return (Math.sqrt(reported.screen_width * reported.screen_width + reported.screen_height * reported.screen_height) / +dpi).toFixed(1);
}

const VIEW_MODES = [
	[ 'full_screen', _('Full screen') ],
	[ 'landscape_window', _('Landscape window') ],
	[ 'portrait_window', _('Portrait window') ]
];

// The density CarPlay is laid out at for a view measured for its picture, on a screen of `density`.
function viewDpi(density, measured) {
	const area = function(size) {
		const m = /^([0-9]+)x([0-9]+)$/.exec(size || '');
		return m ? +m[1] * +m[2] : 0;
	};
	const picture = area(measured.picture), view = area(measured.view);
	if (!(+density > 0) || !picture || !view)
		return null;
	return Math.min(640, Math.max(72, Math.round(+density * Math.sqrt(picture / view))));
}

// A mark after a label or a heading that shows `text` while the pointer rests on it or it has the focus. A tap shows
// it and a second tap, or one anywhere else, hides it again.
function hint(text) {
	return E('span', {
		'class': 'cb-hint', 'tabindex': '0', 'role': 'button', 'aria-label': text,
		'data-tooltip': text, 'data-tooltip-style': 'carbridge-tip',
		'click': function(ev) {
			ev.stopPropagation();
			if (this.hasAttribute('data-open'))
				this.blur();
			else
				this.setAttribute('data-open', '');
		},
		'blur': function() { this.removeAttribute('data-open'); }
	}, 'ⓘ');
}

// Hints close on a tap outside them, and their tooltips stay inside the window.
document.addEventListener('click', function(ev) {
	const open = document.activeElement;
	if (open && open.classList && open.classList.contains('cb-hint') && !open.contains(ev.target))
		open.blur();
}, true);
document.addEventListener('tooltip-open', function(ev) {
	const tip = ev.target;
	if (!tip.classList.contains('carbridge-tip'))
		return;
	const margin = 8;
	const over = tip.getBoundingClientRect().right - (document.documentElement.clientWidth - margin);
	if (over > 0)
		tip.style.left = Math.max(window.pageXOffset + margin, parseFloat(tip.style.left) - over) + 'px';
});

// The page's own styles, on top of the theme's: they take the theme's colours where it has them.
const STYLE = [
	'.cbi-tooltip.carbridge-tip { white-space: pre-line; max-width: min(26em, calc(100vw - 16px)); line-height: 1.6; text-align: left; }',
	'.cb-hint { cursor: help; opacity: 0.6; text-decoration: none; padding: 0 0.3em; margin-inline-start: 0.1em; white-space: nowrap; }',
	'.cb-hint:focus { outline: none; opacity: 1; }',
	'.cb-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr)); gap: 0.75rem; }',
	'.cb-card { min-width: 0; padding: 0.8rem 1rem; border: 1px solid var(--hairline, rgba(128,128,128,0.3)); border-radius: 0.6rem; background: var(--surface-sunken, transparent); }',
	'.cb-card-title { font-size: 0.85em; opacity: 0.7; }',
	'.cb-state { display: flex; align-items: center; gap: 0.5em; margin-top: 0.25em; font-size: 1.15em; font-weight: 600; }',
	'.cb-dot { flex: none; width: 0.6em; height: 0.6em; border-radius: 50%; background: currentColor; opacity: 0.5; }',
	'.cb-good .cb-dot { opacity: 1; color: var(--success, #2e7d32); }',
	'.cb-wait .cb-dot { opacity: 1; color: var(--warning, #c77700); }',
	'.cb-bad .cb-dot { opacity: 1; color: var(--danger, #c62828); }',
	'.cb-detail { margin-top: 0.3em; font-size: 0.85em; opacity: 0.75; overflow-wrap: anywhere; }',
	'.cb-detail.cb-error { opacity: 1; color: var(--danger, #c62828); }',
	'.cb-units { display: flex; flex-direction: column; gap: 0.75rem; }',
	'.cb-unit { display: flex; flex-wrap: wrap; align-items: center; gap: 0.6rem 1rem; }',
	'.cb-unit-main { flex: 1 1 14rem; min-width: 0; }',
	'.cb-unit-name { display: flex; flex-wrap: wrap; align-items: center; gap: 0.4em; font-weight: 600; overflow-wrap: anywhere; }',
	'.cb-chips { display: flex; flex-wrap: wrap; gap: 0.3em; margin-top: 0.4em; }',
	'.cb-chips > .label { border: 1px solid var(--hairline, rgba(128,128,128,0.3)); font-weight: normal; }',
	'.cb-row { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; }',
	'.cb-row > input { flex: 1 1 8rem; max-width: 12rem; min-width: 0; }',
	'.cb-attention { padding: 0.6rem 0.8rem; border-radius: 0.6rem; outline: 2px solid var(--warning, #c77700); }',
	'.cb-pair { display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem; margin-bottom: 0.75rem; }',
	'.cb-pair > .btn { width: 100%; margin: 0; padding-block: 0.7em; }',
	'.cb-pair > .cb-attention { padding: 0.7em 0.8rem; }',
	'.cb-steps { margin: 0 0 1em 1.4em; padding: 0; line-height: 1.8; list-style: decimal outside; }',
	'.cb-steps > li { list-style: inherit; }',
	'.cb-message { margin: 0.8em 0 0; }',
	'.btn.cb-counting:disabled { opacity: 1; cursor: default; font-variant-numeric: tabular-nums; }',
	'#modal_overlay .cb-message.cb-good { color: var(--success, #2e7d32); }',
	'#modal_overlay .cb-message.cb-wait { opacity: 0.85; }',
	'#modal_overlay .cb-message.cb-bad { color: var(--danger, #c62828); }',
	'.cb-log { max-height: min(30em, 60vh); overflow: auto; white-space: pre-wrap; word-break: break-all; font-size: 85%; }',
	'.cb-section-gap { margin-top: 0.75rem; }',
	'.modal .cbi-value-field select, .modal .cbi-value-field input { max-width: 100%; }',
	'@media (max-width: 47.99rem) {',
	'  .cb-unit > .cb-row, .cb-unit > .cb-row > .btn { flex: 1 1 auto; }',
	'  .cb-row > input { max-width: none; }',
	'  .modal .cbi-value-field select, .modal .cbi-value-field input { width: 100%; }',
	'  .modal .right { display: flex; gap: 0.5rem; }',
	'  .modal .right > .btn { flex: 1; }',
	'  .modal .cb-row > .btn { flex: 1 1 auto; }',
	'}'
].join('\n');

function field(label, widgets, help) {
	return E('div', { 'class': 'cbi-value' }, [
		E('label', { 'class': 'cbi-value-title' }, help ? [ label, hint(help) ] : label),
		E('div', { 'class': 'cbi-value-field' }, widgets)
	]);
}

// Turns the descriptions of a form section's options, and the section's own, into hints after their labels, each
// time the section renders.
function withHints(section) {
	const render = section.render;
	section.render = function() {
		return Promise.resolve(render.apply(this, arguments)).then(function(node) {
			node.querySelectorAll('.cbi-value').forEach(function(value) {
				const description = value.querySelector('.cbi-value-description');
				const title = value.querySelector('.cbi-value-title');
				if (description && title && description.textContent.trim()) {
					title.appendChild(hint(description.textContent.trim()));
					description.remove();
				}
			});
			const descr = node.querySelector('.cbi-section-descr');
			const heading = node.querySelector('h3');
			if (descr && heading && descr.textContent.trim()) {
				heading.appendChild(hint(descr.textContent.trim()));
				descr.remove();
			}
			return node;
		});
	};
}

// Opens the head unit's own settings; saving asks CarBridge to apply them. Each setting offers the default, marked,
// which the head unit then follows, and the other values, which it keeps as its own.
function advancedSettings(unit) {
	const own = unit.settings || {};
	const reported = unit.picture;
	const fallback = defaults();
	const reportedSize = reported ? sizeText(automaticSize(reported, unit.views)) : null;

	const autoSize = reportedSize ? _('Automatic (%s)').format(reportedSize) : _('Automatic');
	const sizeChoices = fallback.picture === 'fixed'
		? [ [ 'default', defaultText(sizeText(fallback.size)) ], [ 'auto', autoSize ] ]
		: [ [ 'default', defaultText(autoSize) ] ];
	sizeChoices.push([ 'custom', _('Custom') ]);
	const custom = own.full_screen && own.full_screen !== 'auto';
	const sizeSelect = select(sizeChoices,
		custom ? 'custom' : (own.full_screen === 'auto' && fallback.picture === 'fixed' ? 'auto' : 'default'));
	const sizeInput = E('input', {
		'type': 'text', 'class': 'cbi-input-text',
		'placeholder': reported ? reported.width + 'x' + reported.height : '1280x720',
		'value': custom ? own.full_screen : ''
	});
	const sizeBox = E('div', { 'style': custom ? '' : 'display:none' }, sizeInput);
	sizeSelect.addEventListener('change', function() {
		sizeBox.style.display = sizeSelect.value === 'custom' ? '' : 'none';
	});

	const fpsSelect = select(choicesAround([
		[ 'auto', reported && reported.fps ? _('Automatic (%d fps)').format(reported.fps) : _('Automatic') ],
		[ '30', _('30 fps') ],
		[ '60', _('60 fps') ]
	], fallback.fps), chosen(own.fps, fallback.fps));
	const hevcSelect = select(choicesAround(pairs(HEVC_TEXTS), fallback.hevc), chosen(own.hevc, fallback.hevc));
	const startSelect = select(choicesAround(pairs(START_TEXTS), fallback.start), chosen(own.start, fallback.start));
	const navSelect = select(choicesAround(pairs(NAV_AUDIO_TEXTS), fallback.nav), chosen(own.nav_audio, fallback.nav));

	// The density of the head unit's screen: its real one by default, else the one it lays out with; that one; or
	// one of the owner's. Each mode's density is worked out from it and the view measured in the mode.
	const densities = reportedDensities(reported);
	const withInches = function(text, dpi) {
		const inches = screenInches(reported, dpi);
		return inches ? _('%s (about %s-inch screen)').format(text, inches) : text;
	};
	let realText;
	if (densities.real)
		realText = withInches(_('RealDPI %d').format(+densities.real), densities.real);
	else if (densities.configured)
		realText = _('RealDPI (not reported: DPI %d is used)').format(+densities.configured);
	else
		realText = _('RealDPI');
	const dpiChoices = [ [ 'default', defaultText(realText) ] ];
	dpiChoices.push([ 'configured', densities.configured
		? withInches(_('DPI %d').format(+densities.configured), densities.configured) : _('DPI') ]);
	dpiChoices.push([ 'custom', _('Custom') ]);
	const customDpi = !!own.dpi && own.dpi !== 'configured';
	const dpiSelect = select(dpiChoices, customDpi ? 'custom' : (own.dpi === 'configured' ? 'configured' : 'default'));
	const dpiInput = E('input', {
		'type': 'text', 'class': 'cbi-input-text', 'inputmode': 'numeric',
		'placeholder': densities.real || densities.configured || '160',
		'value': customDpi ? own.dpi : ''
	});
	const dpiBox = E('div', { 'style': customDpi ? '' : 'display:none' }, dpiInput);
	dpiSelect.addEventListener('change', function() {
		dpiBox.style.display = dpiSelect.value === 'custom' ? '' : 'none';
	});
	const density = customDpi ? own.dpi
		: (own.dpi === 'configured' ? (densities.configured || densities.real) : (densities.real || densities.configured));

	// The views measured in each mode, with the density each gives.
	const views = unit.views || {};
	const viewRows = VIEW_MODES.map(function(mode) {
		const measured = views[mode[0]];
		let text = _('Not measured yet');
		if (measured) {
			const dpi = viewDpi(density, measured);
			text = _('%s for the picture %s').format(sizeText(measured.view), sizeText(measured.picture));
			if (dpi)
				text = _('%s, %d dpi').format(text, dpi);
		}
		return field(mode[1], E('div', { 'style': 'padding-top:0.4em' }, text));
	});
	const label = headUnitName(unit);
	const recalibrate = E('button', {
		'class': 'btn cbi-button cbi-button-action',
		'disabled': Object.keys(views).length ? null : '',
		'click': ui.createHandlerFn(this, function() {
			return fs.write(RUN + 'settings', JSON.stringify({ id: unit.id, views: 'default' }) + '\n').then(function() {
				ui.hideModal();
				ui.addNotification(null, E('p', _('The views of %s are measured again: its window modes are calibrated the next time it shows CarPlay in them.').format(label)), 'info');
			});
		})
	}, _('Recalibrate'));

	const error = E('p', { 'class': 'alert-message warning', 'style': 'display:none' });
	ui.showModal(_('Advanced settings for %s').format(label), [
		field(_('Full screen resolution'), [ sizeSelect, sizeBox ], [ _('Automatic: the area the head unit shows CarPlay in, as it reports it, kept for the next start.'), _('Once its touches have measured the full screen view, CarPlay is drawn at that size from the next connection, pixel for pixel.'), _('Other sizes are drawn as they are.'), _('The iPhone connects to CarPlay again when the size changes.') ].join('\n')),
		field(_('Frame rate'), fpsSelect, _('Automatic: the frame rate the head unit asks for.')),
		field(_('H.265 to the head unit'), hevcSelect, [ _('Off: always H.264.'), _('When the head unit offers it: H.265 only for head units that declare it.'), _('Always: H.265 even when undeclared, for head units whose decoder takes it.'), _('A connected iPhone keeps its codec until it connects again.') ].join('\n')),
		field(_('CarPlay starts'), startSelect, [ _('How the cast shows when the head unit connects.'), _('Window: landscape in mirror mode, portrait in simple mode.'), _('The head unit\'s window button switches between full screen and the window.') ].join('\n')),
		field(_('DPI'), [ dpiSelect, dpiBox ], [ _('CarPlay\'s controls grow with the density: the higher, the larger.'), _('• RealDPI: the screen\'s real density, as the head unit reports it'), _('• DPI: the density the head unit gives phones to lay out with'), _('Window modes take their density from the measured views; the first time one is used, tap the screen once to calibrate it.'), _('The iPhone connects to CarPlay again when the density changes.') ].join('\n')),
		field(_('Navigation sound channel'), navSelect, NAV_AUDIO_HINT),
		E('h4', {}, [ _('Measured views'), hint(_('Where CarPlay shows on the head unit\'s screen in each mode, in screen pixels, measured from touches.')) ]),
		E('div', {}, viewRows),
		E('div', { 'class': 'cbi-value' }, [ E('div', { 'class': 'cbi-value-field' }, recalibrate) ]),
		error,
		E('div', { 'class': 'right' }, [
			E('button', { 'class': 'btn', 'click': ui.hideModal }, _('Cancel')),
			' ',
			E('button', {
				'class': 'btn cbi-button cbi-button-save',
				'click': ui.createHandlerFn(this, function() {
					let fullScreen = sizeSelect.value;
					if (fullScreen === 'custom') {
						fullScreen = checkSize(sizeInput.value.trim().replace(/[×X*]/g, 'x'));
						if (!fullScreen) {
							error.textContent = SIZE_ERROR;
							error.style.display = '';
							return;
						}
					}
					let dpi = dpiSelect.value;
					if (dpi === 'custom') {
						dpi = checkDpi(dpiInput.value.trim());
						if (!dpi) {
							error.textContent = DPI_ERROR;
							error.style.display = '';
							return;
						}
					}
					const request = {
						id: unit.id,
						full_screen: fullScreen,
						fps: fpsSelect.value,
						hevc: hevcSelect.value,
						start: startSelect.value,
						dpi: dpi,
						nav_audio: navSelect.value
					};
					return fs.write(RUN + 'settings', JSON.stringify(request) + '\n').then(function() {
						ui.hideModal();
						ui.addNotification(null, E('p', _('The settings of %s are applied in a moment.').format(label)), 'info');
					});
				})
			}, _('Save'))
		])
	]);
}

// Asks CarBridge to forget a head unit, once the user confirms.
function forgetHeadUnit(unit) {
	const label = headUnitName(unit);
	ui.showModal(_('Forget head unit'), [
		E('p', {}, _('Forget %s? CarBridge no longer connects to it until it is paired again with the PIN it shows.').format(label)),
		E('div', { 'class': 'right' }, [
			E('button', { 'class': 'btn', 'click': ui.hideModal }, _('Cancel')),
			' ',
			E('button', {
				'class': 'btn cbi-button cbi-button-negative',
				'click': ui.createHandlerFn(this, function() {
					return fs.write(RUN + 'forget', unit.id + '\n').then(function() {
						ui.hideModal();
						ui.addNotification(null, E('p', _('%s will be forgotten in a moment.').format(label)), 'info');
					});
				})
			}, _('Forget'))
		])
	]);
}

// Reads the status every second while `onStatus` wants it; the function returned stops that.
function watchStatus(onStatus) {
	const tick = function() { return readStatus().then(function(st) { if (st) onStatus(st); }); };
	poll.add(tick, 1);
	return function() { poll.remove(tick); };
}

// A line of a dialog that tells how something goes, drawn anew as it changes.
function message() {
	const node = E('p', { 'class': 'cb-message', 'style': 'display:none' });
	return {
		node: node,
		show: function(text, look) {
			node.className = 'cb-message' + (look ? ' cb-' + look : '');
			node.textContent = text;
			node.style.display = text ? '' : 'none';
		}
	};
}

// How a failed connection of a first pairing is told, by what failed; the PIN is tried again.
const PIN_RETRIES = {
	bluetooth: _('The Bluetooth connection to the head unit failed. Trying again…'),
	wifi: _('Joining the head unit\'s Wi-Fi failed. Trying again…'),
	other: _('The connection to the head unit failed. Trying again…')
};

// How long to wait for a head unit before saying that none is found.
const PIN_FIND_SECONDS = 30;

// Pairs a new head unit: the owner enters the PIN its Carlink screen shows, CarBridge connects to it with the PIN,
// and the dialog tells how that goes. Leaving the dialog before it is paired withdraws the PIN.
function pairHeadUnit(st) {
	let started = null;
	let seen = st && st.pin_pairing ? st.pin_pairing.attempt : 0;
	let done = false;
	const input = E('input', {
		'type': 'text', 'class': 'cbi-input-text', 'inputmode': 'numeric', 'autocomplete': 'one-time-code',
		'maxlength': 6, 'placeholder': '000000', 'style': 'font-size:1.4em; letter-spacing:0.2em; max-width:10em'
	});
	const info = message();
	const connect = E('button', { 'class': 'btn cbi-button cbi-button-apply' }, _('Connect'));
	const close = E('button', { 'class': 'btn' }, _('Cancel'));
	let stop = null;
	const idle = function() {
		started = null;
		connect.disabled = false;
		input.disabled = false;
	};
	const follow = function(st) {
		const outcome = st.pin_pairing;
		if (started === null || done)
			return;
		if (outcome && outcome.attempt > seen) {
			seen = outcome.attempt;
			if (outcome.result === 'paired') {
				const unit = (st.head_units || []).filter(function(u) { return u.id === outcome.head_unit; })[0];
				done = true;
				info.show(_('Paired with %s. It connects in a moment.').format(unit ? headUnitName(unit) : headUnitId(outcome.head_unit)), 'good');
				connect.style.display = 'none';
				close.textContent = _('Done');
				return;
			}
			if (outcome.result === 'refused' || outcome.reason === 'authentication') {
				idle();
				input.value = '';
				info.show(outcome.result === 'refused'
					? _('The PIN is wrong or has expired: the head unit shows a new one from time to time. Enter the one it shows now.')
					: _('The head unit refused the pairing. Enter the PIN it shows now and try again.'), 'bad');
				return;
			}
			info.show(PIN_RETRIES[outcome.reason] || PIN_RETRIES.other, 'wait');
			return;
		}
		if (st.carlink === 'connecting')
			info.show(_('Connecting to the head unit…'), 'wait');
		else if (Date.now() - started > PIN_FIND_SECONDS * 1000)
			info.show(_('No head unit found yet. Keep the head unit on its PIN screen, close to CarBridge.'), 'wait');
		else
			info.show(_('Looking for the head unit…'), 'wait');
	};
	const leave = function() {
		if (stop)
			stop();
		ui.hideModal();
		if (started !== null && !done)
			return L.resolveDefault(fs.remove(RUN + 'pin'), null);
	};
	const start = function() {
		const pin = input.value.trim();
		if (!/^[0-9]{6}$/.test(pin)) {
			info.show(_('The PIN is the six digits the head unit shows.'), 'bad');
			return;
		}
		return fs.write(RUN + 'pin', pin + '\n').then(function() {
			started = Date.now();
			connect.disabled = true;
			input.disabled = true;
			info.show(_('Looking for the head unit…'), 'wait');
			if (!stop)
				stop = watchStatus(follow);
		});
	};
	connect.addEventListener('click', ui.createHandlerFn(this, start));
	close.addEventListener('click', ui.createHandlerFn(this, leave));
	input.addEventListener('keydown', function(ev) {
		if (ev.key === 'Enter') {
			ev.preventDefault();
			if (!connect.disabled)
				start();
		}
	});
	const body = [
		E('ol', { 'class': 'cb-steps' }, [
			E('li', {}, _('On the head unit, open Carlink: it shows a six-digit PIN.')),
			E('li', {}, _('Enter the PIN below and choose Connect.')),
			E('li', {}, _('Keep the head unit on its PIN screen until it is paired.'))
		])
	];
	const connected = linked(st) ? lastHeadUnit(st) : null;
	if (connected)
		body.push(E('p', { 'class': 'cb-detail' }, _('%s is connected now: the new head unit connects once it is not.').format(headUnitName(connected))));
	body.push(E('div', { 'class': 'cb-row' }, input), info.node, E('div', { 'class': 'right' }, [ close, ' ', connect ]));
	ui.showModal(_('Pair a head unit'), body);
	input.focus();
}

// The iPhones paired over Bluetooth, by address.
function iphoneAddresses(st) {
	return ((st && st.carplay && st.carplay.iphones) || []).map(function(phone) { return phone.address; });
}

// Lets iPhones pair: the dialog says how, opens pairing and counts down the time left, and names the iPhones that
// pair meanwhile.
function pairIphone(st) {
	const name = uci.get('carbridge', 'main', 'name') || 'CarBridge';
	const before = iphoneAddresses(st);
	// How long pairing stays open, as CarBridge opens it on request.
	const seconds = +(uci.get('carbridge', 'main', 'pairing_window') || 0) || 300;
	// Until when, by this browser's clock, iPhones can pair; null while they cannot.
	let until = null;
	let asked = 0;
	// Failures to open pairing so far, for a new one to be told.
	let failures = st && st.carplay && st.carplay.pairing_error ? st.carplay.pairing_error.attempt : 0;
	const button = E('button', { 'class': 'btn cbi-button cbi-button-action' });
	const info = message();
	const problem = message();
	const show = function() {
		const left = until === null ? 0 : Math.ceil((until - Date.now()) / 1000);
		button.disabled = left > 0;
		button.classList.toggle('cb-counting', left > 0);
		if (left > 0)
			button.textContent = _('iPhones can pair: %d s').format(left);
		else {
			until = null;
			button.textContent = _('Start pairing an iPhone');
		}
	};
	const follow = function(st) {
		const open = st.carplay ? st.carplay.pairing_until : 0;
		if (open > 0 && until === null) {
			const left = open - Date.now() / 1000;
			until = left > 0 && left <= 3600 ? Date.now() + left * 1000 : Date.now() + seconds * 1000;
		}
		else if (!open && until !== null && Date.now() - asked > 5000)
			until = null;
		const added = ((st.carplay && st.carplay.iphones) || []).filter(function(phone) { return before.indexOf(phone.address) < 0; });
		if (added.length)
			info.show(_('Paired with %s.').format(added.map(function(phone) { return phone.name || phone.address; }).join(', ')), 'good');
		const failed = st.carplay && st.carplay.pairing_error;
		const unavailable = st.carplay && st.carplay.unavailable;
		if (failed && failed.attempt > failures) {
			failures = failed.attempt;
			until = null;
			problem.show(_('Pairing could not be opened: %s').format(failed.detail), 'bad');
		}
		else if (unavailable && unavailable.reason !== 'off')
			problem.show(_('CarPlay cannot run now, so iPhones cannot pair: %s').format(UNAVAILABLE_TEXTS[unavailable.reason] || unavailable.reason), 'bad');
		show();
	};
	const stop = watchStatus(follow);
	const timer = setInterval(show, 1000);
	button.addEventListener('click', ui.createHandlerFn(this, function() {
		problem.show('');
		return fs.write(RUN + 'pair', '1\n').then(function() {
			asked = Date.now();
			until = Date.now() + seconds * 1000;
			show();
		});
	}));
	const close = E('button', {
		'class': 'btn',
		'click': function() {
			stop();
			clearInterval(timer);
			ui.hideModal();
		}
	}, _('Close'));
	ui.showModal(_('Pair an iPhone'), [
		E('ol', { 'class': 'cb-steps' }, [
			E('li', {}, _('Choose Start pairing an iPhone below.')),
			E('li', {}, _('On the iPhone, open Settings > Bluetooth, tap %s under Other Devices and pair.').format(name)),
			E('li', {}, _('A few seconds later the iPhone asks to connect CarPlay: tap Connect.'))
		]),
		E('div', { 'class': 'cb-row' }, button),
		info.node,
		problem.node,
		E('div', { 'class': 'right' }, close)
	]);
	follow(st || {});
}

// Asks CarBridge to unpair an iPhone, once the user confirms.
function unpairIphone(phone) {
	const label = phone.name || phone.address;
	ui.showModal(_('Unpair iPhone'), [
		E('p', {}, _('Unpair %s? It no longer connects to CarBridge. To use it again, forget CarBridge in the iPhone\'s Bluetooth settings, then pair it again.').format(label)),
		E('div', { 'class': 'right' }, [
			E('button', { 'class': 'btn', 'click': ui.hideModal }, _('Cancel')),
			' ',
			E('button', {
				'class': 'btn cbi-button cbi-button-negative',
				'click': ui.createHandlerFn(this, function() {
					return fs.write(RUN + 'forget-iphone', phone.address + '\n').then(function() {
						ui.hideModal();
						ui.addNotification(null, E('p', _('%s will be unpaired in a moment.').format(label)), 'info');
					});
				})
			}, _('Unpair'))
		])
	]);
}

function iphoneList(st) {
	const phones = (st && st.carplay && st.carplay.iphones) || [];
	if (!phones.length)
		return E('p', {}, _('No iPhone paired yet.'));
	return E('div', { 'class': 'cb-units' }, phones.map(function(phone) {
		const name = [ phone.name || phone.address ];
		if (phone.connected)
			name.push(E('span', { 'class': 'label success' }, _('Connected')));
		return E('div', { 'class': 'cb-card cb-unit' }, [
			E('div', { 'class': 'cb-unit-main' }, [
				E('div', { 'class': 'cb-unit-name' }, name),
				E('div', { 'class': 'cb-detail' }, _('Bluetooth %s').format(phone.address))
			]),
			E('div', { 'class': 'cb-row' }, E('button', {
				'class': 'btn cbi-button cbi-button-remove',
				'click': function() { unpairIphone(phone); }
			}, _('Unpair')))
		]);
	}));
}

// The Carlink states in which a head unit is connected.
function linked(st) {
	return !!st && (st.carlink === 'connected' || st.carlink === 'casting');
}

// The head unit connected last, if one is known.
function lastHeadUnit(st) {
	return ((st && st.head_units) || []).filter(function(unit) { return unit.last; })[0] || null;
}

// Short notes on the settings a head unit has of its own.
function ownSettingNotes(unit) {
	const own = unit.settings || {};
	const notes = [];
	const note = function(label, value) { notes.push(_('%s: %s').format(label, value)); };
	if (own.full_screen)
		note(_('Full screen resolution'), own.full_screen === 'auto' ? _('Automatic') : sizeText(own.full_screen));
	if (own.fps)
		note(_('Frame rate'), own.fps === 'auto' ? _('Automatic') : _('%d fps').format(+own.fps));
	if (own.hevc)
		note(_('H.265 to the head unit'), HEVC_TEXTS[own.hevc] || own.hevc);
	if (own.start)
		note(_('CarPlay starts'), START_TEXTS[own.start] || own.start);
	if (own.nav_audio)
		note(_('Navigation sound channel'), NAV_AUDIO_TEXTS[own.nav_audio] || own.nav_audio);
	if (own.dpi) {
		const configured = reportedDensities(unit.picture).configured;
		notes.push(own.dpi === 'configured' ? (configured ? _('DPI %d').format(+configured) : _('DPI')) : _('DPI %d').format(+own.dpi));
	}
	return notes;
}

// A time in Unix seconds as the local date and time, to the minute.
function dateText(seconds) {
	const d = new Date(seconds * 1000);
	return '%04d-%02d-%02d %02d:%02d'.format(d.getFullYear(), d.getMonth() + 1, d.getDate(), d.getHours(), d.getMinutes());
}

function headUnitList(st) {
	const units = (st && st.head_units) || [];
	if (!units.length)
		return E('p', {}, _('No head unit paired yet.'));
	// Names more than one head unit has, which are told apart by their IDs.
	const shared = {};
	units.forEach(function(unit) { shared[headUnitName(unit)] = shared.hasOwnProperty(headUnitName(unit)); });
	return E('div', { 'class': 'cb-units' }, units.map(function(unit) {
		const name = [ headUnitName(unit) ];
		if (unit.last)
			name.push(linked(st)
				? E('span', { 'class': 'label success' }, _('Connected'))
				: E('span', { 'class': 'label' }, _('Last connected')));
		const main = [ E('div', { 'class': 'cb-unit-name' }, name) ];
		if (unit.name && shared[unit.name])
			main.push(E('div', { 'class': 'cb-detail' }, _('ID %s').format(headUnitId(unit.id))));
		if (unit.paired)
			main.push(E('div', { 'class': 'cb-detail' }, _('Paired %s').format(dateText(unit.paired))));
		const notes = ownSettingNotes(unit);
		if (notes.length)
			main.push(E('div', { 'class': 'cb-chips' }, notes.map(function(text) {
				return E('span', { 'class': 'label' }, text);
			})));
		return E('div', { 'class': 'cb-card cb-unit' }, [
			E('div', { 'class': 'cb-unit-main' }, main),
			E('div', { 'class': 'cb-row' }, [
				E('button', {
					'class': 'btn cbi-button cbi-button-edit',
					'click': function() { advancedSettings(unit); }
				}, _('Advanced')),
				E('button', {
					'class': 'btn cbi-button cbi-button-remove',
					'click': function() { forgetHeadUnit(unit); }
				}, _('Forget'))
			])
		]);
	}));
}

function card(title, state, look, details) {
	return E('div', { 'class': 'cb-card' }, [
		E('div', { 'class': 'cb-card-title' }, title),
		E('div', { 'class': look ? 'cb-state cb-' + look : 'cb-state' }, [ E('span', { 'class': 'cb-dot' }), E('span', {}, state) ])
	].concat(details || []));
}

function detail(text, error) {
	return E('div', { 'class': error ? 'cb-detail cb-error' : 'cb-detail' }, text);
}

// The largest file of an MFi identity CarBridge takes.
const MFI_FILE_BYTES = 16384;

// A file the browser picked, as base64.
function readBase64(file) {
	return new Promise(function(resolve, reject) {
		const reader = new FileReader();
		reader.onload = function() {
			const url = String(reader.result);
			resolve(url.substring(url.indexOf(',') + 1));
		};
		reader.onerror = function() { reject(reader.error); };
		reader.readAsDataURL(file);
	});
}

// How each file of the MFi identity is, by the state the status gives.
const MFI_FILE_TEXTS = {
	missing: _('Missing'),
	unreadable: _('Cannot be read'),
	present: _('Present')
};

// Why the MFi identity cannot be used, by the problem the status gives.
const MFI_PROBLEMS = {
	missing: _('A file of the certificate is missing.'),
	unreadable: _('A file of the certificate cannot be read.'),
	'bad-key': _('The private key cannot be used: it must be a P-256 key in PKCS #8.'),
	'bad-certificate': _('The certificate cannot be used: it must be PKCS #7 holding one P-256 certificate.'),
	mismatch: _('The private key does not belong to the certificate.')
};

// How an upload of an MFi identity went, from the outcome the status gives.
function mfiOutcome(install) {
	if (install.result === 'installed')
		return [ install.restart_needed
			? _('Installed. CarPlay uses it after Restart CarPlay.')
			: _('Installed. CarPlay starts in a moment.'), 'good' ];
	if (install.result === 'failed')
		return [ _('Installing failed: %s').format(install.detail || ''), 'bad' ];
	if (install.reason === 'present')
		return [ _('A usable certificate is installed: confirm replacing it.'), 'bad' ];
	if (install.reason === 'upload')
		return [ _('The files could not be read: upload the private key and the certificate in DER, PEM or base64.'), 'bad' ];
	return [ MFI_PROBLEMS[install.reason] || install.detail || install.reason, 'bad' ];
}

// Shows the MFi identity CarPlay authenticates with, and lets the owner upload one: CarBridge checks it before it
// installs it, and replaces a usable one only when the owner confirms it.
function mfiCertificate(st) {
	const mfi = (st && st.carplay && st.carplay.mfi) || {};
	let seen = mfi.install ? mfi.install.attempt : 0;
	const rows = [
		field(_('Private key'), E('div', { 'style': 'padding-top:0.4em' }, 'identity.pk8: ' + (MFI_FILE_TEXTS[mfi.key] || '-'))),
		field(_('Certificate'), E('div', { 'style': 'padding-top:0.4em' }, 'certificate.p7b: ' + (MFI_FILE_TEXTS[mfi.certificate] || '-'))),
		field(_('State'), E('div', { 'style': 'padding-top:0.4em', 'class': mfi.usable ? '' : 'cb-error' },
			mfi.usable ? _('Usable') : (MFI_PROBLEMS[mfi.problem] || mfi.problem_detail || _('Unknown'))))
	];
	const info = mfi.certificate_info;
	if (info) {
		const text = function(value) { return E('div', { 'style': 'padding-top:0.4em; overflow-wrap:anywhere' }, value); };
		rows.push(field(_('Subject'), text(info.subject)));
		rows.push(field(_('Issuer'), text(info.issuer)));
		rows.push(field(_('Serial number'), text(info.serial)));
		rows.push(field(_('Valid'), text(_('%s to %s').format(dateText(info.not_before), dateText(info.not_after)))));
	}
	const keyInput = E('input', { 'type': 'file', 'accept': '.pk8,.der,.pem,.key' });
	const certificateInput = E('input', { 'type': 'file', 'accept': '.p7b,.p7c,.der,.pem' });
	const replace = E('input', { 'type': 'checkbox' });
	const status = message();
	const install = E('button', { 'class': 'btn cbi-button cbi-button-apply' }, mfi.usable ? _('Replace') : _('Install'));
	let stop = null;
	const follow = function(st) {
		const outcome = st.carplay && st.carplay.mfi && st.carplay.mfi.install;
		if (!outcome || outcome.attempt <= seen)
			return;
		seen = outcome.attempt;
		install.disabled = false;
		const told = mfiOutcome(outcome);
		status.show(told[0], told[1]);
	};
	install.addEventListener('click', ui.createHandlerFn(this, function() {
		const key = keyInput.files[0], certificate = certificateInput.files[0];
		if (!key || !certificate) {
			status.show(_('Choose both the private key and the certificate.'), 'bad');
			return;
		}
		if (key.size > MFI_FILE_BYTES || certificate.size > MFI_FILE_BYTES) {
			status.show(_('The files are too large for an MFi certificate.'), 'bad');
			return;
		}
		if (mfi.usable && !replace.checked) {
			status.show(_('A usable certificate is installed: confirm replacing it.'), 'bad');
			return;
		}
		install.disabled = true;
		status.show(_('Checking the certificate…'), 'wait');
		return Promise.all([ readBase64(key), readBase64(certificate) ]).then(function(files) {
			return fs.write(RUN + 'mfi-identity.pk8', files[0] + '\n')
				.then(function() { return fs.write(RUN + 'mfi-certificate.p7b', files[1] + '\n'); })
				.then(function() { return fs.write(RUN + 'mfi-install', (mfi.usable ? 'replace' : 'install') + '\n'); });
		}).then(function() {
			if (!stop)
				stop = watchStatus(follow);
		}).catch(function(err) {
			install.disabled = false;
			status.show(_('The files cannot be sent: %s').format(err.message || err), 'bad');
		});
	}));
	const upload = [
		E('h4', {}, [ mfi.usable ? _('Replace the certificate') : _('Upload a certificate'),
			hint(_('CarBridge checks that both files can be read and that the key belongs to the certificate before it installs them. The private key is never shown.')) ]),
		field(_('Private key'), keyInput),
		field(_('Certificate'), certificateInput)
	];
	if (mfi.usable)
		upload.push(field(_('Replace'), E('label', {}, [ replace, ' ', _('Replace the certificate CarPlay uses now') ])));
	ui.showModal(_('MFi certificate'), [
		E('div', {}, rows),
		E('div', {}, upload),
		status.node,
		E('div', { 'class': 'right' }, [
			E('button', {
				'class': 'btn',
				'click': function() {
					if (stop)
						stop();
					ui.hideModal();
				}
			}, _('Close')),
			' ',
			install
		])
	]);
}

// Restarts CarBridge, once the user confirms.
function restartCarPlay() {
	ui.showModal(_('Restart CarPlay'), [
		E('p', {}, _('Restart CarBridge now? CarPlay and the picture on the head unit drop for a few seconds, then connect again.')),
		E('div', { 'class': 'right' }, [
			E('button', { 'class': 'btn', 'click': ui.hideModal }, _('Cancel')),
			' ',
			E('button', {
				'class': 'btn cbi-button cbi-button-negative',
				'click': ui.createHandlerFn(this, function() {
					return callInitAction('carbridge', 'restart').then(function(done) {
						ui.hideModal();
						ui.addNotification(null, E('p', done ? _('CarBridge is restarting.') : _('CarBridge could not be restarted.')), done ? 'info' : 'error');
					});
				})
			}, _('Restart'))
		])
	]);
}

// The status at a glance: the head unit's connection and the CarPlay receiver; `openMfi` opens the MFi certificate's
// dialog.
function dashboard(st, openMfi) {
	if (!st)
		return card(_('Status'), _('CarBridge is not running.'), 'bad');

	const linkDetails = [];
	const unit = lastHeadUnit(st);
	if (linked(st) && unit)
		linkDetails.push(detail(headUnitName(unit)));
	if (st.carlink === 'pin-needed')
		linkDetails.push(detail(_('Choose Pair a head unit below and enter it.')));
	if (st.carlink_error && !linked(st))
		linkDetails.push(detail(st.carlink_error, true));

	const carplay = st.carplay || {};
	const unavailable = carplay.unavailable && carplay.unavailable.reason !== 'off' ? carplay.unavailable : null;
	const noMfi = (unavailable && unavailable.reason === 'mfi') || (carplay.mfi && carplay.mfi.key && !carplay.mfi.usable);
	const mfiButton = E('div', { 'class': 'cb-row cb-section-gap' }, E('button', {
		'class': 'btn cbi-button ' + (noMfi ? 'cbi-button-action cb-attention' : 'cbi-button-neutral'),
		'click': function() { openMfi(); }
	}, _('MFi certificate')));
	let carplayCard;
	if (uci.get('carbridge', 'main', 'carplay') === '0')
		carplayCard = card(_('CarPlay status'), _('Off'), null, [ detail(_('The head unit shows CarBridge\'s waiting screen.')) ]);
	else if (unavailable) {
		const details = [ detail(UNAVAILABLE_TEXTS[unavailable.reason] || unavailable.reason, true) ];
		if (unavailable.detail)
			details.push(detail(unavailable.detail));
		details.push(detail(unavailable.reason === 'mfi'
			? _('Upload the certificate with MFi certificate below.')
			: _('CarBridge tries again every few seconds.')));
		carplayCard = card(_('CarPlay status'), _('Unavailable'), 'bad', details.concat([ mfiButton ]));
	}
	else if (!carplay.receiver)
		carplayCard = card(_('CarPlay status'), _('Not running'), 'bad', [ mfiButton ]);
	else if (!carplay.iphone)
		carplayCard = card(_('CarPlay status'), _('Waiting for an iPhone'), null, [ mfiButton ]);
	else
		carplayCard = card(_('CarPlay status'), carplay.screen ? _('Showing CarPlay') : _('iPhone connected'), 'good', [ mfiButton ]);

	return E('div', { 'class': 'cb-grid' }, [
		card(_('Status'), CARLINK_STATES[st.carlink] || st.carlink, CARLINK_LOOKS[st.carlink], linkDetails),
		carplayCard
	]);
}

function showVersion(node, st) {
	node.textContent = (st && st.version) || '';
	node.style.display = node.textContent ? '' : 'none';
}

// A button that shows and hides what `nodes` gives, collapsed at first.
function toggle(showText, hideText, nodes, onShow) {
	let shown = false;
	const button = E('button', { 'type': 'button', 'class': 'btn cbi-button cbi-button-neutral' }, showText);
	button.addEventListener('click', function(ev) {
		ev.preventDefault();
		shown = !shown;
		button.textContent = shown ? hideText : showText;
		nodes().forEach(function(node) { node.style.display = shown ? '' : 'none'; });
		if (shown && onShow)
			onShow();
	});
	return { button: button, shown: function() { return shown; } };
}

return view.extend({
	load: function() {
		return Promise.all([ readStatus(), readAdapters(), uci.load('carbridge') ]).then(function(loaded) {
			return [ loaded[0], loaded[1] ];
		});
	},

	render: function(loaded) {
		const status = loaded[0];
		const adapters = loaded[1];
		// The status as the page shows it now, for the dialogs that start from it.
		let latest = status;
		const openMfi = function() { mfiCertificate(latest); };
		const dashboardNode = E('div', {}, dashboard(status, openMfi));
		const versionNode = E('span', { 'class': 'label' });
		showVersion(versionNode, status);
		const headUnitsNode = E('div', {}, headUnitList(status));
		const iphonesNode = E('div', {}, iphoneList(status));

		const pairHeadUnitButton = E('button', {
			'class': 'btn cbi-button cbi-button-action',
			'click': function() { pairHeadUnit(latest); }
		}, _('Pair a head unit'));
		const pairIphoneButton = E('button', {
			'class': 'btn cbi-button cbi-button-action',
			'click': function() { pairIphone(latest); }
		}, _('Pair an iPhone'));

		// The detailed log's files, the oldest first, joined on the router.
		const downloadDetailLog = function() {
			return fs.exec_direct(DETAIL_LOG_READER, [], 'blob').then(function(blob) {
				saveBlob(blob, 'carbridge-detail.log');
			}).catch(function(err) {
				ui.addNotification(null, E('p', {}, _('The detailed log cannot be downloaded: %s').format(err.message || err)), 'error');
			});
		};
		const detailLogButton = E('button', {
			'class': 'btn cbi-button cbi-button-neutral',
			'style': detailLogOn() ? '' : 'display:none',
			'click': ui.createHandlerFn(this, downloadDetailLog)
		}, _('Download detailed log'));

		// The status as last shown: the parts of the page are drawn again only when it changes, so that a tap is not
		// lost to a button drawn anew under it.
		let shown = null;
		const update = function(st) {
			latest = st;
			const text = JSON.stringify([ st, uci.get('carbridge', 'main', 'carplay') ]);
			if (text !== shown) {
				shown = text;
				dashboardNode.replaceChildren(dashboard(st, openMfi));
				showVersion(versionNode, st);
				headUnitsNode.replaceChildren(headUnitList(st));
				iphonesNode.replaceChildren(iphoneList(st));
			}
			pairHeadUnitButton.classList.toggle('cb-attention', !!st && st.carlink === 'pin-needed');
			detailLogButton.style.display = detailLogOn() ? '' : 'none';
		};
		update(status);
		poll.add(function() {
			return readStatus().then(update);
		}, 3);

		const logNode = E('pre', { 'class': 'cb-log' }, _('Loading…'));
		const showLog = function() {
			return readLog().then(function(text) {
				logNode.textContent = text || _('No log yet.');
				logNode.scrollTop = logNode.scrollHeight;
			});
		};
		const downloadLog = function() {
			return readLog(Infinity).then(function(text) {
				saveBlob(new Blob([ text + '\n' ], { type: 'text/plain' }), 'carbridge.log');
			});
		};
		const logBox = E('div', { 'style': 'display:none' }, [
			logNode,
			E('div', { 'class': 'cb-row' }, [
				E('button', { 'class': 'btn cbi-button cbi-button-neutral', 'click': ui.createHandlerFn(this, showLog) }, _('Refresh')),
				E('button', { 'class': 'btn cbi-button cbi-button-neutral', 'click': ui.createHandlerFn(this, downloadLog) }, _('Download')),
				detailLogButton
			])
		]);
		const logToggle = toggle(_('Show log'), _('Hide log'), function() { return [ logBox ]; }, showLog);
		const restartButton = E('button', {
			'class': 'btn cbi-button cbi-button-negative',
			'click': function() { restartCarPlay(); }
		}, _('Restart CarPlay'));

		// The settings an owner sets once, then the advanced ones, collapsed until asked for. The advanced sections
		// stay as they were shown when the form renders again after saving.
		const m = new form.Map('carbridge');
		let s = m.section(form.NamedSection, 'main', 'carbridge', _('Settings'));
		withHints(s);
		s.option(form.Flag, 'enabled', _('Enabled'));
		s.option(form.Value, 'name', _('Name'), _('The name the iPhone and the head unit show.'));
		// The adapters of the iPhone's access point are left out; a chosen adapter that is not connected stays listed.
		let o = s.option(form.ListValue, 'carlink_mac', _('Head unit Wi-Fi adapter'),
			[ _('The Wi-Fi adapter that joins the head unit\'s network: a second one, besides the iPhone\'s access point.'), _('CarBridge leaves it out of the router\'s own Wi-Fi and names its interface carlink0.'), _('It is known by its MAC address, on whichever USB port.') ].join('\n'));
		o.value('', _('Not chosen'));
		adapters.forEach(function(adapter) {
			if (!adapter.ap)
				o.value(adapter.mac, adapterText(adapter));
		});
		const chosenMac = uci.get('carbridge', 'main', 'carlink_mac') || '';
		if (chosenMac && !adapters.some(function(adapter) { return adapter.mac === chosenMac.toLowerCase(); }))
			o.value(chosenMac, _('%s (not connected)').format(chosenMac));
		o.rmempty = true;

		const advancedNodes = function() {
			return Array.prototype.slice.call(document.querySelectorAll('.carbridge-advanced'));
		};
		const advancedToggle = toggle(_('Show advanced settings'), _('Hide advanced settings'), advancedNodes);
		const collapsible = function(section, withButton) {
			const render = section.render;
			section.render = function() {
				return Promise.resolve(render.apply(this, arguments)).then(function(node) {
					const box = E('div', {
						'class': 'carbridge-advanced',
						'style': advancedToggle.shown() ? '' : 'display:none'
					}, node);
					return withButton ? E('div', {}, [ E('p', {}, advancedToggle.button), box ]) : box;
				});
			};
		};

		s = m.section(form.NamedSection, 'main', 'carbridge', _('Advanced settings'));
		withHints(s);
		collapsible(s, true);
		s.option(form.Flag, 'carplay', _('CarPlay'), _('Off: the head unit shows CarBridge\'s waiting screen only.'));
		o = s.option(form.Value, 'pairing_window', _('Pairing after start'), _('Seconds iPhones can pair after CarBridge starts; 0 for only with the button.'));
		o.datatype = 'uinteger';
		o = s.option(form.Value, 'carlink_iface', _('Head unit Wi-Fi interface'), _('Without a chosen adapter: the Wi-Fi interface that joins the head unit\'s network, as it is named.'));
		o.depends('carlink_mac', '');
		s.option(form.Value, 'ap_iface', _('iPhone access point'), _('The access point the iPhone joins for CarPlay.'));
		s.option(form.Value, 'lan_iface', _('iPhone network interface'));
		s.option(form.Value, 'adapter', _('Bluetooth adapter'));
		const detailLogDir = uci.get('carbridge', 'main', 'detail_log_dir') || DETAIL_LOG_DIR;
		s.option(form.Flag, 'detail_log', _('Detailed log'),
			[ _('Also writes the log, with statistics of the microphone, of sound dropped and of the link to the head unit, to %s on the router\'s storage.').format(detailLogDir), _('10 MB at most: the oldest lines give way to new ones.'), _('Turning it on or off does not restart CarBridge.') ].join('\n'));

		s = m.section(form.NamedSection, 'main', 'carbridge', _('Defaults for head units'),
			_('For head units without settings of their own. A head unit\'s own settings are under Advanced in the list of head units.'));
		withHints(s);
		collapsible(s, false);
		o = s.option(form.ListValue, 'carplay_picture', _('CarPlay picture size'),
			[ _('Automatic: the area the head unit shows CarPlay in, as it reports it, kept for the next start.'), _('Once its touches have measured the full screen view, CarPlay is drawn at that size from the next connection, pixel for pixel.'), _('Other sizes are drawn as they are.'), _('The iPhone connects to CarPlay again when the size changes.') ].join('\n'));
		o.value('auto', defaultText(_('Automatic')));
		o.value('fixed', _('Fixed'));
		o.default = 'auto';
		o = s.option(form.Value, 'carplay_size', _('Fixed picture size'), [ _('Width × height in pixels, for example 1280x720.'), _('Also used with Automatic until a head unit is known.') ].join('\n'));
		o.datatype = 'string';
		o.placeholder = '1280x720';
		o.validate = function(id, value) {
			return checkSize(value) ? true : SIZE_ERROR;
		};
		o = s.option(form.ListValue, 'carplay_fps', _('Frame rate'),
			_('Automatic: the frame rate the head unit asks for.'));
		o.value('auto', defaultText(_('Automatic')));
		o.value('30', _('30 fps'));
		o.value('60', _('60 fps'));
		o.default = 'auto';
		o = s.option(form.ListValue, 'carplay_start', _('CarPlay starts'),
			[ _('How the cast shows when the head unit connects.'), _('Window: landscape in mirror mode, portrait in simple mode.'), _('The head unit\'s window button switches between full screen and the window.') ].join('\n'));
		o.value('fullscreen', defaultText(START_TEXTS.fullscreen));
		o.value('window', START_TEXTS.window);
		o.default = 'fullscreen';
		o = s.option(form.ListValue, 'hevc', _('H.265 to the head unit'),
			[ _('Off: always H.264.'), _('When the head unit offers it: H.265 only for head units that declare it.'), _('Always: H.265 even when undeclared, for head units whose decoder takes it.'), _('A connected iPhone keeps its codec until it connects again.') ].join('\n'));
		o.value('off', HEVC_TEXTS.off);
		o.value('auto', defaultText(HEVC_TEXTS.auto));
		o.value('force', _('Always, even when the head unit does not offer it'));
		o.default = 'auto';
		o = s.option(form.ListValue, 'nav_audio', _('Navigation sound channel'), NAV_AUDIO_HINT);
		o.value('tts', defaultText(NAV_AUDIO_TEXTS.tts));
		o.value('media', NAV_AUDIO_TEXTS.media);
		o.value('call', NAV_AUDIO_TEXTS.call);
		o.value('assistant', NAV_AUDIO_TEXTS.assistant);
		o.default = 'tts';

		return m.render().then(function(settings) {
			return E('div', {}, [
				E('style', {}, STYLE),
				E('h2', {}, [ _('CarBridge'), ' ', versionNode ]),
				E('div', { 'class': 'cbi-section' }, dashboardNode),
				E('div', { 'class': 'cbi-section' }, [
					E('h3', {}, _('Head units')),
					E('div', { 'class': 'cb-pair' }, [ pairHeadUnitButton, pairIphoneButton ]),
					headUnitsNode
				]),
				E('div', { 'class': 'cbi-section' }, [
					E('h3', {}, _('Paired iPhones')),
					iphonesNode
				]),
				settings,
				E('div', { 'class': 'cbi-section' }, [
					E('h3', {}, [ _('Log'), hint(_('The latest %d lines CarBridge logged.').format(LOG_LINES)) ]),
					E('div', { 'class': 'cb-row' }, [ logToggle.button, restartButton ]),
					logBox
				])
			]);
		});
	}
});
