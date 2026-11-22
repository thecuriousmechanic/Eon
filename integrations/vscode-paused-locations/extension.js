'use strict';

const vscode = require('vscode');
const { register } = require('./command');
const allocations = require('./allocations');

let counts;
exports.activate = context => {
  counts = allocations.register(vscode);
  context.subscriptions.push(register(vscode), ...counts.subscriptions);
};
exports.deactivate = () => counts?.close();
