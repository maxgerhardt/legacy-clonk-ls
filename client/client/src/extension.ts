import * as fs from 'fs';
import * as path from 'path';
import { commands, ExtensionContext, extensions, OutputChannel, QuickPickItem, window, workspace } from 'vscode';

import {
	LanguageClient,
	LanguageClientOptions,
	ServerOptions,
	TransportKind
} from 'vscode-languageclient/node';
import { C4Group } from './c4group';
import { ScenarioRunner } from './runner';
import { CONFIG_NAME } from './config';

let client: LanguageClient;

export function activate(context: ExtensionContext) {
	const outputChannel = window.createOutputChannel('Legacy Clonk');
	configureTreeViewCommands(context, outputChannel);
	configureIdSelectCommand(context, outputChannel);
	configureAndStartLanguageServer(context);
}

function configureAndStartLanguageServer(context: ExtensionContext) {

	// Packaged binaries: lsp.exe (Windows x64), lsp-linux-x64, lsp-darwin-arm64 (see .github/workflows/vsix.yml)
	const executableName = process.platform === 'win32' ? 'lsp.exe' : `lsp-${process.platform}-${process.arch}`;

	let pathToBin = context.asAbsolutePath(
		path.join('client', 'out', executableName)
	);
	if (!fs.existsSync(pathToBin)) {
		// locally built extension with a single unsuffixed binary
		pathToBin = context.asAbsolutePath(path.join('client', 'out', process.platform === 'win32' ? 'lsp.exe' : 'lsp'));
	}

	const pathToBinInDebug = context.asAbsolutePath(
		path.join('..', 'server', 'target', 'debug', process.platform === 'win32' ? 'lsp.exe' : 'lsp')
	);

	// The executable bit can get lost when the server binary is packaged into the .vsix
	if (process.platform !== 'win32') {
		try {
			fs.chmodSync(pathToBin, 0o755);
		} catch (e) {
			// not fatal: the server start below reports the actual problem
		}
	}

	// The game folder lets the server index the packs scenarios load (Objects.c4d, Knights.c4d, ...)
	const gameExecutable = workspace.getConfiguration(CONFIG_NAME).get<string>('pathToGameExecutable');
	const typeChecks = workspace.getConfiguration(CONFIG_NAME).get<boolean>('typeChecks', true);
	const fieldTypes = workspace.getConfiguration(CONFIG_NAME).get<boolean>('fieldTypes', true);
	const objectIds = workspace.getConfiguration(CONFIG_NAME).get<boolean>('objectIds', true);
	const nilChecks = workspace.getConfiguration(CONFIG_NAME).get<boolean>('nilChecks', false);
	const assumeCreateSucceeds = workspace.getConfiguration(CONFIG_NAME).get<boolean>('assumeCreateSucceeds', true);
	const stringTableLanguages = workspace.getConfiguration(CONFIG_NAME).get<string[]>('stringTableLanguages', ['DE', 'US']);
	const initializationOptions = gameExecutable
		? { gameDir: path.dirname(gameExecutable), typeChecks, fieldTypes, objectIds, nilChecks, assumeCreateSucceeds, stringTableLanguages }
		: { typeChecks, fieldTypes, objectIds, nilChecks, assumeCreateSucceeds, stringTableLanguages };

	const serverOptions: ServerOptions = {
		run: {
			command: pathToBin,
			args: [],
			transport: TransportKind.stdio,
		},
		debug: {
			command: pathToBinInDebug,
			args: ["--debug"],
			transport: TransportKind.stdio,
		},
	};

	const clientOptions: LanguageClientOptions = {
		documentSelector: [{
			scheme: 'file',
			language: 'c4script',
		}, {
			scheme: 'file',
			language: 'c4ini',
		}],
		initializationOptions,
	};

	client = new LanguageClient(
		'legacyClonkLanguageServer',
		'Legacy Clonk Language Server',
		serverOptions,
		clientOptions
	);

	client.start();
	client.info("Client started");

	// the server reads these once (initializationOptions): offer a reload when one changes
	const serverSettings = ['pathToGameExecutable', 'typeChecks', 'fieldTypes', 'objectIds', 'nilChecks', 'assumeCreateSucceeds', 'stringTableLanguages'];
	context.subscriptions.push(workspace.onDidChangeConfiguration(e => {
		if (serverSettings.some(s => e.affectsConfiguration(CONFIG_NAME + '.' + s))) {
			window.showInformationMessage('Legacy Clonk: reload the window to apply the changed language server setting.', 'Reload Window')
				.then(choice => { if (choice) { commands.executeCommand('workbench.action.reloadWindow'); } });
		}
	}));
}

function configureTreeViewCommands(context: ExtensionContext, outputChannel: OutputChannel) {
	const c4group = new C4Group(outputChannel);
	const runner = new ScenarioRunner();

	context.subscriptions.push(commands.registerCommand(CONFIG_NAME + '.unpackC4g', ({ fsPath }) => {
		c4group.unpack(fsPath)
			.then(_ => commands.executeCommand("workbench.files.action.refreshFilesExplorer"));
	}));

	context.subscriptions.push(commands.registerCommand(CONFIG_NAME + '.packC4g', ({ fsPath }) => {
		c4group.pack(fsPath)
			.then(_ => commands.executeCommand("workbench.files.action.refreshFilesExplorer"));
	}));

	context.subscriptions.push(commands.registerCommand(CONFIG_NAME + '.runScenarioInEditor', ({ fsPath }) => {
		runner.run(fsPath, outputChannel);
	}));
}

function configureIdSelectCommand(context: ExtensionContext, outputChannel: OutputChannel) {
	context.subscriptions.push(commands.registerCommand(CONFIG_NAME + '.pasteId', () => {

		client.sendRequest('x-legacy-clonk/getAllIds')
			.then(response => {

				if (!Array.isArray(response)) {
					outputChannel.appendLine(`Value returned from language server was expected to be an array but was ${typeof response}`);
					return;
				}

				outputChannel.appendLine(`x-legacy-clonk/getAllIds returned ${response.length} results`);

				const items: QuickPickItem[] = [];

				// Validate?
				for (const v of response) {
					items.push({
						label: v.id,
						detail: v.tlc,
						description: v.name,
					});
				}

				return window.showQuickPick(items, {
					title: 'this is title',
					matchOnDescription: true,
				});
			}).then(item => {
				if (typeof item === 'object') {
					const editor = window.activeTextEditor;

					if (!editor) return;

					return editor.edit(b => {
						b.replace(editor.selection, item.label);
					});
				}
			});
	}));
}

export function deactivate(): Thenable<void> | undefined {
	if (!client) {
		return undefined;
	}
	return client.stop();
}
