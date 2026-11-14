import { field, type FieldCopy } from './types';

export const auth = {
  setup: {
    title: 'Welcome to RemoteWeather',
    blurb: "Let's get your weather station online. This takes about 5 minutes.",
    step1: 'Create your administrator account',
    codeHeading: 'The setup code',
    codeNote:
      'RemoteWeather printed a one-time setup code to the console when it started. Paste it here. It works once.',
    codeMissing:
      "Can't find it? Run `modernweather reset-setup-token` on this machine to print a new one.",
    create: 'Create my account',
    creating: 'Creating…',
    claimed: {
      heading: 'Already set up',
      body: 'This machine already has an administrator account. Sign in instead.'
    }
  },
  signIn: {
    title: 'Sign in',
    submit: 'Sign in',
    submitting: 'Signing in…',
    /* One message for both wrong-username and wrong-password: naming which was wrong tells
       anyone who asks which usernames exist. */
    failed: "That username and password don't match.",
    rateLimited: 'Too many attempts. Wait a moment and try again.',
    signedOut: 'You have been signed out.',
    sessionExpired: 'Your session expired. Sign in again to carry on.'
  },
  firstRun: {
    heading: 'Getting started',
    collapsed: 'Getting started — all done',
    dismiss: 'Hide this',
    steps: {
      account: 'Create your administrator account',
      station: 'Connect a weather station',
      readings: 'Check that readings are arriving',
      storage: 'Choose where to keep your readings',
      website: 'Put your weather on the web',
      share: 'Share your data with weather services'
    },
    optional: 'optional',
    done: 'done',
    youAreHere: 'you are here',
    doneWhen:
      'The first three are what matter. The rest stay here until you do them, and nothing nags.'
  },
  fields: {
    setupCode: field({
      label: 'Setup code',
      hint: 'The code RemoteWeather printed to the console when it started.',
      more: 'It is printed to the console only and is never written to the log, so the only copy is the one you can see. Dashes and spaces are ignored and lower case is fine.'
    }),
    username: field({
      label: 'Username',
      hint: 'What you will sign in with. Anything you will remember.'
    }),
    password: field({
      label: 'Password',
      hint: 'At least 12 characters. A short phrase you can type beats a short jumble you cannot.'
    }),
    passwordAgain: field({
      label: 'Password again',
      hint: 'Type it a second time, so a typo does not lock you out.'
    })
  } satisfies Record<string, FieldCopy>
};
