import { render, createRef, Component } from 'preact';
import * as monaco from 'monaco-editor';
import { signal } from '@preact/signals';
//import Editor from '@monaco-editor/react';


// https://github.com/vitejs/vite/discussions/1791#discussioncomment-9281911
import editorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import jsonWorker from "monaco-editor/esm/vs/language/json/json.worker?worker";
import cssWorker from "monaco-editor/esm/vs/language/css/css.worker?worker";
import htmlWorker from "monaco-editor/esm/vs/language/html/html.worker?worker";
import tsWorker from "monaco-editor/esm/vs/language/typescript/ts.worker?worker";

self.MonacoEnvironment = {
  getWorker(_: any, label: string) {
    if (label === "json") {
      return new jsonWorker();
    }
    if (label === "css" || label === "scss" || label === "less") {
      return new cssWorker();
    }
    if (label === "html" || label === "handlebars" || label === "razor") {
      return new htmlWorker();
    }
    if (label === "typescript" || label === "javascript") {
      return new tsWorker();
    }
    return new editorWorker();
  },
};

import './style.css';

type File = {
    name: string,
    data: string,
};

const PLACEHOLDER_DIRECTIONS = `Run something to see the output...
`;
const initialFiles: File[] = [
  {
    name: 'f1/script.js',
    data: 'let a = 10;',
  },
  {
    name: 'style.css',
    data: 'body { border: 10px; }',
  },
  {
    name: 'index.html',
    data: '<div>hi</div>',
  },
];

class ModelStore {
    models = signal([]); // monaco models
    prefix: string;

    constructor(prefix) {
        this.prefix = prefix || '/';
        // on reload we populate from the global list of models
        this.models.value = monaco.editor.getModels().filter(m => m.uri.path.startsWith(prefix));
    }

    getOrCreateModel(name, text) {
        let m = this.getModel(name);
        if (m) {
            m.setValue(text);
            return m;
        }
        let uri = monaco.Uri.file(this.prefix + name);
        m = monaco.editor.createModel(text, undefined, uri);
        this.models.value = [...this.models.value, m];
        return m;
    }

    getModel(name) {
        let uri = monaco.Uri.file(this.prefix + name);
        return monaco.editor.getModel(uri);
    }

    clear() {
        for (let model in this.models.value) {
            model.dispose();
        }
        this.models.value = [];
    }

    deleteModel(name) {
        let model = this.getModel(name);
        if (m) {
            this.models.value = this.models.value.filter(m => m !== model);
            model.dispose();
        } else {
            console.warn(`tried deleting model on ${this.prefix}/${name}`);
        }
    }

    modelName(model) {
        return model.uri.path.slice(this.prefix.length);
    }
}

// signals.
// is this good or terrible? I don't know
const s_images = signal({});
const s_chosenImage = signal(null);
const s_entrypoint = signal('');
const s_cmd = signal('');
const inputModelStore = new ModelStore("/input/")
const outputModelStore = new ModelStore("/output/")

if (inputModelStore.models.value.length === 0) {
    for (let f of initialFiles) {
        inputModelStore.getOrCreateModel(f.name, f.data);
    }
}

class Editor extends Component {
    ref = createRef();
    editor: any;
    store: ModelStore;
    readOnly: bool;

    constructor({readOnly,placeholder,store}) {
        super();
        this.placeholder = placeholder ?? null;
        this.readOnly = readOnly ?? false;
        this.store = store;
        console.log(this.store.prefix, this.readOnly)
    }

    componentDidMount() {
        this.editor = monaco.editor.create(this.ref.current, {
            placeholder: this.placeholder,
            readOnly: this.readOnly,
        });
        if (this.store.models.value.length > 0) {
            this.editFile(this.store.models.value[0].uri);
        }
    }

    editFile(uri) {
        this.editor.updateOptions({placeholder: ''});
        this.editor.setModel(monaco.editor.getModel(uri));
    }

    // this.props, this.state
    render() {
        let models = this.store.models.value;
        let tabs = models.map((model) => {
            return (
                <button key={model.id} onClick={() => this.editFile(model.uri)}>
                    {this.store.modelName(model)}
                </button>);
        });
        return (
            <div class="editorContainer">
                {tabs}
                <div class="monacoContainer" ref={this.ref}></div>
            </div>
        );
    }
}

class ImageForm extends Component {
    render() {
        let imageOptions = s_images.value.images?.map(({info,links}) => {
            let name = `${info.registry}/${info.repository}/${info.tag}`;
            return <option key={links.runi} value={links.runi}>{name}</option>;
        });
        let stdinOptions = inputModelStore.models.value.map(m => {
            let name = inputModelStore.modelName(m);
            return <option key={m.uri.path} value={name}>{name}</option>;
        });
        //let entrypoint = s_chosenImage.value?.config.config.Entrypoint || 'Entrypoint';
        //let cmd = s_chosenImage.value?.config.config.cmd || 'Cmd';

        let entrypoint = 'Entrypoint';
        let cmd = 'Cmd';
        return (
            <>
            <select name="image" onChange={(event) => { s_chosenImage.value = event.currentTarget.value; }}>
                {imageOptions}
            </select>
            <input type="text" name="entrypoint" placeholder={entrypoint} />
            <input type="text" name="cmd" placeholder={cmd} />
            <select name="stdin" onChange={(event) => { s_chosenImage.value = event.currentTarget.value; }}>
                <option value="">/dev/null</option>
                {stdinOptions}
            </select>
            </>
        );
    }
}

class App extends Component {
    constructor({initialFiles}) {
        super();
    }

    async run(event) {
        event.preventDefault();
        console.log('hi this should run');
        // okay I need to get the current image, entrypoint, cmd, stdin
        // the image should already be the uri

        if (s_chosenImage.value !== null) {
            let data = {};
            // TODO get entrypoint and cmd from child
            data.cmd = ["sh", "-c", "echo hi"];
            let req = new Request(window.location.origin + s_chosenImage.value, {
                method: 'POST',
                body: JSON.stringify(data),
                headers: {
                    'Content-type': 'application/json',
                }
            });
            const response = await fetch(req);
            if (response.ok) {
                const json = await response.json();
                let m = outputModelStore.getOrCreateModel('output.json', JSON.stringify(json, null, '  '));
                outputEditor.editFile(m.uri);
            } else {
                console.error(response);
            }
        } else {
            console.warn('cannot execute without a chosen image');
        }
    }

    // this.props, this.state
    render({}, {}) {
        return <div>
            <form>
                <ImageForm />
                <button onClick={this.run}>Run</button>
            </form>

            <div id="editorSideBySide">
                <Editor
                    store={inputModelStore}
                />
                <Editor
                    store={outputModelStore}
                    placeholder={PLACEHOLDER_DIRECTIONS}
                    readOnly={true}
                />
            </div>
        </div>
    }
}

// TODO something about the args going in not being compat with the Component type signature
render(<App/>, document.getElementById('app'));

const response = await fetch(window.location.origin + '/api/v1/images');
console.log(response);
if (response.ok) {
    const json = await response.json();
    s_images.value = json;
    if (json.images?.length > 0) {
        s_chosenImage.value = json.images[0].links.runi;
    }
} else {
    console.error(response);
}
//<Editor
//  height="80vh"
//  theme="vs-dark"
//  path={file.name}
//  defaultLanguage={file.language}
//  defaultValue={file.value}
///>
//function getOrCeateFile(path, text) {
//    let uri = monaco.Uri.file(path);
//    return monaco.editor.getModel(uri) || monaco.editor.createModel(text, undefined, uri);
//}

