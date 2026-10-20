import * as fs from 'fs';
import * as path from 'path';
import {
	CancellationToken, commands, debug, DebugAdapterDescriptor, DebugAdapterExecutable, DebugConfiguration,
	DebugConfigurationProvider, ExtensionContext, ProviderResult, window, workspace, WorkspaceFolder
} from 'vscode';
import { CONFIG_NAME } from './config';

// Script debugger: the debug adapter c4dap (packaged next to the language server) drives an engine started with
// /debug:<port> (LegacyClonk built from the fork's debugger branch).

function adapterPath(context: ExtensionContext): string {
	const name = process.platform === 'win32' ? 'c4dap.exe' : `c4dap-${process.platform}-${process.arch}`;
	let p = context.asAbsolutePath(path.join('client', 'out', name));
	if (!fs.existsSync(p)) {
		p = context.asAbsolutePath(path.join('client', 'out', process.platform === 'win32' ? 'c4dap.exe' : 'c4dap'));
	}
	if (process.platform !== 'win32') {
		try { fs.chmodSync(p, 0o755); } catch (e) { /* the start reports it */ }
	}
	return p;
}

function engineSetting(): string | undefined {
	const config = workspace.getConfiguration(CONFIG_NAME);
	return config.get<string>('debugEngine') || config.get<string>('pathToGameExecutable') || undefined;
}

// the .c4s folder a file lies in
function scenarioOf(file: string | undefined): string | undefined {
	for (let dir = file; dir && path.dirname(dir) !== dir; dir = path.dirname(dir)) {
		if (dir.toLowerCase().endsWith('.c4s')) {
			return dir;
		}
	}
	return undefined;
}

class C4ScriptConfigurationProvider implements DebugConfigurationProvider {
	resolveDebugConfiguration(folder: WorkspaceFolder | undefined, config: DebugConfiguration, token?: CancellationToken): ProviderResult<DebugConfiguration> {
		// F5 without launch.json: debug the scenario of the open script
		if (!config.type && !config.request && !config.name) {
			const scenario = scenarioOf(window.activeTextEditor?.document.uri.fsPath);
			if (!scenario) {
				window.showInformationMessage('Open a script inside a scenario (.c4s) or add a launch configuration to debug.');
				return undefined;
			}
			config.type = 'c4script';
			config.request = 'launch';
			config.name = 'Debug scenario';
			config.scenario = scenario;
		}
		if (config.request === 'launch') {
			if (!config.engine) {
				config.engine = engineSetting();
			}
			if (!config.engine) {
				window.showErrorMessage(`Set ${CONFIG_NAME}.debugEngine (or pathToGameExecutable) to a clonk.exe with the script debugger.`);
				return undefined;
			}
			if (!config.scenario) {
				config.scenario = scenarioOf(window.activeTextEditor?.document.uri.fsPath);
			}
		}
		return config;
	}
}

export function configureDebugger(context: ExtensionContext) {
	context.subscriptions.push(debug.registerDebugAdapterDescriptorFactory('c4script', {
		createDebugAdapterDescriptor(): ProviderResult<DebugAdapterDescriptor> {
			return new DebugAdapterExecutable(adapterPath(context));
		}
	}));
	context.subscriptions.push(debug.registerDebugConfigurationProvider('c4script', new C4ScriptConfigurationProvider()));
	context.subscriptions.push(commands.registerCommand(CONFIG_NAME + '.debugScenario', (uri?: { fsPath: string }) => {
		const scenario = scenarioOf(uri?.fsPath ?? window.activeTextEditor?.document.uri.fsPath);
		if (!scenario) {
			window.showInformationMessage('Pick a scenario (.c4s) or a script inside one.');
			return;
		}
		debug.startDebugging(workspace.workspaceFolders?.[0], { type: 'c4script', request: 'launch', name: 'Debug ' + path.basename(scenario), scenario });
	}));
}
