import { render, Component } from 'preact';
import * as monaco from 'monaco-editor';
import Editor from '@monaco-editor/react';

import preactLogo from './assets/preact.svg';
import './style.css';

//export function App() {
//	return (
//		<div>
//			<a href="https://preactjs.com" target="_blank">
//				<img src={preactLogo} alt="Preact logo" height="160" width="160" />
//			</a>
//			<h1>Get Started building Vite-powered Preact Apps </h1>
//			<section>
//				<Resource
//					title="Learn Preact"
//					description="If you're new to Preact, try the interactive tutorial to learn important concepts"
//					href="https://preactjs.com/tutorial"
//				/>
//				<Resource
//					title="Differences to React"
//					description="If you're coming from React, you may want to check out our docs to see where Preact differs"
//					href="https://preactjs.com/guide/v10/differences-to-react"
//				/>
//				<Resource
//					title="Learn Vite"
//					description="To learn more about Vite and how you can customize it to fit your needs, take a look at their excellent documentation"
//					href="https://vitejs.dev"
//				/>
//			</section>
//		</div>
//	);
//}
//
//function Resource(props) {
//	return (
//		<a href={props.href} target="_blank" class="resource">
//			<h2>{props.title}</h2>
//			<p>{props.description}</p>
//		</a>
//	);
//}

//export function App() {
//    render (
//        <div>
//            <div id="editor"></div>
//        </div>
//    );
//}

type File = {
    name: string,
    language: string,
    value: string,
};

type Files = {[name: string]: File};

const files: Files = {
  'script.js': {
    name: 'script.js',
    language: 'javascript',
    value: 'let a = 10;',
  },
  'style.css': {
    name: 'style.css',
    language: 'css',
    value: 'body { border: 10px; }',
  },
  'index.html': {
    name: 'index.html',
    language: 'html',
    value: '<div>hi</div>',
  },
};

class App extends Component {
    state: {
        file: File,
        files: Files,
    };

    constructor({files: Files}) {
        super();
        console.log(files);
        this.state = {
            files: files,
            file: files['script.js'],
        };
    }

    setFileName(s) {
        this.setState({file: files[s]});
    }

    // this.props, this.state
    render({}, {file, fileName}) {
        return <div>
          <button disabled={file.name === 'script.js'} onClick={() => this.setFileName('script.js')}>
            script.js
          </button>
          <button disabled={file.name === 'style.css'} onClick={() => this.setFileName('style.css')}>
            style.css
          </button>
          <button disabled={file.name === 'index.html'} onClick={() => this.setFileName('index.html')}>
            index.html
          </button>
          <Editor
            height="80vh"
            theme="vs-dark"
            path={file.name}
            defaultLanguage={file.language}
            defaultValue={file.value}
          />
        </div>
    }
}

// TODO something about the args going in not being compat with the Component type signature
// @ts-ignore
render(<App files={files} />, document.getElementById('app'));
