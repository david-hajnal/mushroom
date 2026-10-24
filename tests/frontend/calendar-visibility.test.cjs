const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function frontend() {
  const context = vm.createContext({
    URL,
    localStorage: { getItem: () => null },
  });
  const source = fs.readFileSync(path.join(__dirname, '../../static/assets/app.js'), 'utf8');
  vm.runInContext(source.replace(/init\(\);\s*$/, ''), context);
  return (code) => JSON.parse(JSON.stringify(vm.runInContext(code, context)));
}

test('restored events offer calendar choices before discovery and can be hidden', () => {
  const run = frontend();
  run(`state.dashboard = {calendar: {events: [
    {id: 'https://cal.test/Work%20Calendar/:uid:2026-10-24T10:00:00Z'},
    {id: 'https://cal.test/Work%20Calendar/:other:2026-10-24T11:00:00Z'},
    {id: 'https://cal.test/Home/:uid:2026-10-24T12:00:00Z'}
  ]}}`);
  assert.deepEqual(run('calendarChoices()'), [
    ['https://cal.test/Work%20Calendar/', 'Work Calendar'],
    ['https://cal.test/Home/', 'Home'],
  ]);
  run(`hiddenCalendars.add('https://cal.test/Work%20Calendar/')`);
  assert.equal(run('visibleCalendarSection(state.dashboard.calendar).events.length'), 1);
  assert.equal(run('calendarChoices().length'), 2);
  run(`calendars = [['https://cal.test/Work%20Calendar/', 'Office'], ['https://cal.test/Empty/', 'Empty']]`);
  assert.deepEqual(run('calendarChoices()'), [
    ['https://cal.test/Work%20Calendar/', 'Office'],
    ['https://cal.test/Empty/', 'Empty'],
    ['https://cal.test/Home/', 'Home'],
  ]);
});
